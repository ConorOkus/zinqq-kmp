//! `Node`'s background tasks: the six `spawn_*_task` loops the node starts
//! under its runtime, plus the reconnect-target lookup one of them reads each
//! tick. Split out of `node.rs` so the fund-safety lifecycle (start/stop/
//! restore/fence ordering) stays readable next to the queries, rather than
//! sharing one ~1,800-line `impl` block with every feature surface.
//!
//! These are ordinary inherent methods on [`Node`]: an `impl` block may live in
//! any module of the defining crate, and a child module can already see its
//! parent's private items, so nothing here needed its visibility widened.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use bitcoin::secp256k1::PublicKey;
use lightning::chain::Confirm;
use lightning::log_error;
use lightning::util::logger::Logger as _;
use tokio::runtime::Runtime;
use tokio::sync::watch;

use crate::builder::NodeComponents;
use crate::close_records;
use crate::liquidity::LiquiditySource;
use crate::node::{
    CoreEvent, Node, FEE_UPDATE_INTERVAL, LIGHTNING_SYNC_INTERVAL, ONCHAIN_SYNC_INTERVAL,
    PEER_RECONNECT_INTERVAL, RGS_SYNC_INTERVAL,
};
use crate::recovery::{RecoverySweeper, AUTO_RECOVER_EVERY_TICKS, RECOVERY_TICK_INTERVAL};
use crate::sweep::{SweepEngine, SWEEP_RETRY_EVERY_TICKS, SWEEP_TICK_INTERVAL};
use crate::util::{hex_str, now_ms};

impl Node {
    pub(super) fn spawn_broadcast_task(
        &self,
        runtime: &Runtime,
        components: &NodeComponents,
        mut stop_receiver: watch::Receiver<()>,
    ) {
        let broadcaster = Arc::clone(&components.broadcaster);
        let chain_source = Arc::clone(&components.chain_source);
        runtime.spawn(async move {
            let mut queue = broadcaster.queue().await;
            loop {
                tokio::select! {
                    _ = stop_receiver.changed() => return,
                    package = queue.recv() => match package {
                        Some(package) => chain_source.process_broadcast_package(package).await,
                        None => return,
                    },
                }
            }
        });
    }

    pub(super) fn spawn_sync_task(
        &self,
        runtime: &Runtime,
        components: &NodeComponents,
        mut stop_receiver: watch::Receiver<()>,
        chain_synced: Arc<AtomicBool>,
        onchain_sync_paused: Arc<AtomicBool>,
        onchain_sync_now: Arc<tokio::sync::Notify>,
    ) {
        let chain_source = Arc::clone(&components.chain_source);
        let channel_manager = Arc::clone(&components.channel_manager);
        let chain_monitor = Arc::clone(&components.chain_monitor);
        let onchain_wallet = Arc::clone(&components.onchain_wallet);
        let gossip_source = Arc::clone(&components.gossip_source);
        let dual_kv_store = Arc::clone(&components.dual_kv_store);
        let logger = Arc::clone(&components.logger);
        let event_sink = Arc::clone(&self.event_sink);
        let close_records = Arc::clone(&self.close_records);
        let sweep_store = Arc::clone(&self.sweep_store);

        runtime.spawn(async move {
            let mut lightning_interval = tokio::time::interval(LIGHTNING_SYNC_INTERVAL);
            lightning_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut onchain_interval = tokio::time::interval(ONCHAIN_SYNC_INTERVAL);
            onchain_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut fee_interval = tokio::time::interval(FEE_UPDATE_INTERVAL);
            fee_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut rgs_interval = tokio::time::interval(RGS_SYNC_INTERVAL);
            rgs_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

            loop {
                tokio::select! {
                    _ = stop_receiver.changed() => return,
                    _ = lightning_interval.tick() => {
                        // U11/KTD-8: no OutputSweeper confirmable — the
                        // descriptor-store pipeline verifies its broadcasts
                        // against chain truth itself.
                        let confirmables: Vec<&(dyn Confirm + Sync + Send)> = vec![
                            &*channel_manager,
                            &*chain_monitor,
                        ];
                        let now_synced = match chain_source.sync_confirmables(confirmables).await {
                            Ok(()) => true,
                            Err(e) => {
                                log_error!(logger, "Background chain sync failed: {e}");
                                false
                            }
                        };
                        let was_synced = chain_synced.swap(now_synced, Ordering::AcqRel);
                        if was_synced != now_synced {
                            event_sink.emit(if now_synced {
                                CoreEvent::ChainSyncCompleted
                            } else {
                                CoreEvent::ChainSyncFailed
                            });
                        }
                        // U3 (KTD-3 CM semantics): a channel-manager write
                        // whose bounded VSS attempt failed stashed its exact
                        // bytes; this tick resends THOSE bytes (never a fresh
                        // encode — a byte-stable retry is what lets a lost
                        // acknowledgement's 409 be recognised as our own write
                        // instead of fencing the wallet), without ever blocking
                        // the background processor.
                        if dual_kv_store.cm_dirty() {
                            dual_kv_store.retry_cm_pending().await;
                        }
                        // U10: chain-truth reconcile for close records rides
                        // the sync tick (the PWA's onSynced extension point).
                        // Budgeted (8 first-party Esplora queries), zero-cost
                        // in the no-pending-closes steady state.
                        if now_synced {
                            let open_ids: HashSet<String> = channel_manager
                                .list_channels()
                                .iter()
                                .map(|details| hex_str(&details.channel_id.0))
                                .collect();
                            close_records::reconcile_close_records(
                                &close_records,
                                &*chain_source,
                                &*onchain_wallet,
                                &open_ids,
                                // U11: channels with un-swept outputs block
                                // completion — a partial sweep's receipt
                                // must not complete the record early.
                                // Resolved lazily past the steady-state check.
                                || sweep_store.pending_channel_ids(),
                                now_ms(),
                                &logger,
                            )
                            .await;
                        }
                    }
                    _ = onchain_interval.tick() => {
                        // U8: skipped while a send is building/signing so the
                        // sync never steps on the wallet mid-send.
                        if onchain_sync_paused.load(Ordering::Acquire) {
                            continue;
                        }
                        // U8: a pass that changed wallet-visible data is the
                        // ONLY thing that tells the shells to re-query the
                        // on-chain balance and activity; an unchanged pass
                        // stays silent so a quiet wallet gets no 120 s
                        // heartbeat event.
                        match chain_source.sync_onchain_wallet(&onchain_wallet).await {
                            Ok(true) => event_sink.emit(CoreEvent::OnchainStateChanged),
                            Ok(false) => {}
                            Err(e) => log_error!(logger, "On-chain wallet sync failed: {e}"),
                        }
                    }
                    _ = onchain_sync_now.notified() => {
                        // U8: the post-broadcast immediate sync (PWA syncNow)
                        // — runs regardless of the pause flag, exactly like
                        // the PWA's in-window syncNow.
                        match chain_source.sync_onchain_wallet(&onchain_wallet).await {
                            Ok(true) => event_sink.emit(CoreEvent::OnchainStateChanged),
                            Ok(false) => {}
                            Err(e) => log_error!(logger, "Post-broadcast wallet sync failed: {e}"),
                        }
                    }
                    _ = fee_interval.tick() => {
                        // U12/KTD-9: the tick only polls; the 60 s cache TTL
                        // and 15 s failure backoff decide whether a refresh
                        // actually runs.
                        if chain_source.fee_refresh_due() {
                            if let Err(e) = chain_source.update_fee_rate_estimates().await {
                                log_error!(logger, "Fee rate update failed: {e}");
                            }
                        }
                    }
                    _ = rgs_interval.tick() => {
                        if let Err(e) = gossip_source.update_rgs_snapshot().await {
                            log_error!(logger, "RGS snapshot update failed: {e}");
                        }
                    }
                }
            }
        });
    }

    /// The static half of the reconnect set (U12): the configured peers.
    /// U3's known-peers store contributes the dynamic half inside the loop
    /// (read per tick, so peers saved during the session get dialed). The
    /// configured LSP is part of the reconnect set too, but is dialed via
    /// `LiquiditySource::ensure_lsp_connected` (see the loop body) so its
    /// dial lock is honored.
    fn reconnect_targets(&self) -> Vec<crate::config::PeerInfo> {
        self.config.peers.clone()
    }

    pub(super) fn spawn_peer_reconnect_task(
        &self,
        runtime: &Runtime,
        components: &NodeComponents,
        liquidity_source: Arc<LiquiditySource>,
        mut stop_receiver: watch::Receiver<()>,
    ) {
        // Ordinary peers are dialed here; the LSP goes through
        // `LiquiditySource::ensure_lsp_connected`, which holds the dial lock a
        // racing `receive_jit` also takes. Both firing at t=0 otherwise opens
        // two connections and LDK drops one, which can strand an in-flight
        // LSPS2 request on the dropped socket.
        let peers = self.reconnect_targets();
        let peer_manager = Arc::clone(&components.peer_manager);
        let known_peers = Arc::clone(&components.known_peers);
        runtime.spawn(async move {
            let mut interval = tokio::time::interval(PEER_RECONNECT_INTERVAL);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = stop_receiver.changed() => return,
                    _ = interval.tick() => {
                        let connected: HashSet<PublicKey> = peer_manager
                            .list_peers()
                            .iter()
                            .map(|details| details.counterparty_node_id)
                            .collect();
                        // U3: configured peers plus the saved known peers,
                        // read fresh each tick, deduplicated by node id.
                        let mut targets = peers.clone();
                        targets.extend(known_peers.reconnect_targets());
                        let mut dialed: HashSet<PublicKey> = HashSet::new();
                        for peer in &targets {
                            if !dialed.insert(peer.node_id) {
                                continue;
                            }
                            if !connected.contains(&peer.node_id) {
                                if let Some(connection) = lightning_net_tokio::connect_outbound(
                                    Arc::clone(&peer_manager),
                                    peer.node_id,
                                    peer.address,
                                )
                                .await
                                {
                                    tokio::spawn(connection);
                                }
                            }
                        }
                        if let Err(error) = liquidity_source.ensure_lsp_connected().await {
                            log_error!(
                                liquidity_source.logger(),
                                "LSP reconnect attempt failed: {error}"
                            );
                        }
                    }
                }
            }
        });
    }

    /// U10 recovery ticks (PWA `context.tsx:1535-1553`): the exit reconcile
    /// runs EVERY ~10 s tick (cheap — in-memory record lookups) so a false
    /// "deposit needed" banner clears within a sync cycle of the close
    /// record healing; the sweep-based auto-recovery (U11's engine via the
    /// [`RecoverySweeper`] seam) runs every ~60 s.
    pub(super) fn spawn_recovery_task(
        &self,
        runtime: &Runtime,
        sweeper: Arc<dyn RecoverySweeper>,
        mut stop_receiver: watch::Receiver<()>,
    ) {
        let recovery = Arc::clone(&self.recovery);
        let close_records = Arc::clone(&self.close_records);
        runtime.spawn(async move {
            let mut interval = tokio::time::interval(RECOVERY_TICK_INTERVAL);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut tick_count: u32 = 0;
            loop {
                tokio::select! {
                    _ = stop_receiver.changed() => return,
                    _ = interval.tick() => {
                        recovery.maybe_clear_resolved(&close_records);
                        tick_count = tick_count.wrapping_add(1);
                        if tick_count.is_multiple_of(AUTO_RECOVER_EVERY_TICKS) {
                            recovery.maybe_auto_recover(
                                &*sweeper,
                                now_ms(),
                            ).await;
                        }
                    }
                }
            }
        });
    }

    /// U11 sweep cadence (PWA `context.tsx:1495-1563` + the startup sweep,
    /// `event-handler.ts:189-213`): the first tick fires immediately (crash
    /// recovery for descriptors persisted by a previous run), then retries
    /// run hourly — or every 60 s while only incoming on-chain funds block a
    /// subsidized sweep, so a fresh deposit is picked up promptly. The
    /// `SpendableOutputs` event arm wakes an immediate pass via `sweep_now`.
    pub(super) fn spawn_sweep_task(
        &self,
        runtime: &Runtime,
        engine: Arc<SweepEngine>,
        sweep_now: Arc<tokio::sync::Notify>,
        mut stop_receiver: watch::Receiver<()>,
    ) {
        let store = Arc::clone(&self.sweep_store);
        runtime.spawn(async move {
            let mut interval = tokio::time::interval(SWEEP_TICK_INTERVAL);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut tick: u32 = 0;
            loop {
                tokio::select! {
                    _ = stop_receiver.changed() => return,
                    _ = sweep_now.notified() => {
                        engine.sweep_once().await;
                    }
                    _ = interval.tick() => {
                        let due = tick == 0
                            || store.needs_onchain_funds()
                            || tick.is_multiple_of(SWEEP_RETRY_EVERY_TICKS);
                        tick = tick.wrapping_add(1);
                        if due {
                            engine.sweep_once().await;
                        }
                    }
                }
            }
        });
    }

    /// Pumps `LiquidityManager` events into the [`LiquiditySource`], which
    /// resolves the pending get_info/buy awaits (U4).
    pub(super) fn spawn_liquidity_event_task(
        &self,
        runtime: &Runtime,
        components: &NodeComponents,
        liquidity_source: Arc<LiquiditySource>,
        mut stop_receiver: watch::Receiver<()>,
    ) {
        let liquidity_manager = Arc::clone(&components.liquidity_manager);
        runtime.spawn(async move {
            loop {
                tokio::select! {
                    _ = stop_receiver.changed() => return,
                    event = liquidity_manager.next_event_async() => {
                        liquidity_source.handle_liquidity_event(event);
                    }
                }
            }
        });
    }
}
