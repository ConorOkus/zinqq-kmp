//! Node lifecycle (KTD-3, KTD-10): the `Node` owns a 2-worker tokio runtime
//! created at `start()` and dropped at `stop()`. The background processor runs
//! via `process_events_async_with_kv_store_sync` with
//! `mobile_interruptable_platform = true`; periodic chain sync, fee refresh,
//! RGS refresh, broadcast draining, and peer reconnects run as runtime tasks
//! stopped through watch channels.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use bitcoin::secp256k1::PublicKey;
use lightning::chain::Confirm;
use lightning::events::{Event, ReplayEvent};
use lightning::log_error;
use lightning::log_info;
use lightning::util::logger::Logger as _;
use lightning_background_processor::{process_events_async_with_kv_store_sync, GossipSync};
use tokio::runtime::Runtime;
use tokio::sync::watch;

use crate::builder::{build, persist_channel_manager, BuildError, NodeComponents};
use crate::config::{
    Config, PeerInfo, FEE_UPDATE_INTERVAL, LIGHTNING_SYNC_INTERVAL, ONCHAIN_SYNC_INTERVAL,
    PEER_RECONNECT_INTERVAL, RGS_SYNC_INTERVAL,
};
use crate::liquidity::{LiquiditySource, Lsps2Error};
use crate::payment::{describe_failure_reason, send_bolt11, SendError};
use crate::types::{Logger, Sweeper};

/// Internal core events, mapped into the public FFI `Event` enum by the
/// persisted event queue (the [`EventSink`] seam).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CoreEvent {
    /// A background chain sync pass reached the tip.
    ChainSyncCompleted,
    /// A background chain sync pass failed; it will be retried.
    ChainSyncFailed,
    /// A JIT invoice is ready to display (U4).
    InvoiceReady {
        bolt11: String,
        expiry_unix_secs: u64,
    },
    /// An inbound payment was durably claimed (U4). `skimmed_fee_msat` is the
    /// JIT opening fee the LSP withheld, observed on the claimable event.
    PaymentReceived {
        amount_msat: u64,
        skimmed_fee_msat: Option<u64>,
    },
    /// An inbound (JIT) channel is pending confirmation.
    ChannelPending,
    /// An inbound (JIT) channel is usable.
    ChannelReady,
    /// The LSPS2 flow failed (U4).
    Lsps2Failed { reason: String },
    /// An outbound payment succeeded (U5): LDK holds the preimage receipt.
    PaymentSuccessful,
    /// An outbound payment failed terminally (U5). `reason` is either the
    /// stringified LDK failure reason or the synchronous attempt failure.
    PaymentFailed { reason: String },
}

/// Consumer of [`CoreEvent`]s (U3 seam).
pub(crate) trait EventSink: Send + Sync {
    fn emit(&self, event: CoreEvent);
}

struct LoggingEventSink {
    logger: Arc<Logger>,
}

impl EventSink for LoggingEventSink {
    fn emit(&self, event: CoreEvent) {
        log_info!(self.logger, "Core event: {event:?}");
    }
}

struct RunningState {
    runtime: Runtime,
    components: NodeComponents,
    liquidity_source: Arc<LiquiditySource>,
    /// Stops the sync/broadcast/reconnect/liquidity tasks.
    stop_sender: watch::Sender<()>,
    /// Stops the background processor (which persists on the way out).
    bp_stop_sender: watch::Sender<()>,
    bp_handle: tokio::task::JoinHandle<()>,
    chain_synced: Arc<AtomicBool>,
}

/// A foreground-only mainnet LDK node over the wallet-core stack.
///
/// There is deliberately no way to construct a `Node` from an existing seed or
/// mnemonic (AE2): the only input is a [`Config`], and entropy is generated
/// into the storage dir on first start.
pub struct Node {
    config: Config,
    state: Mutex<Option<RunningState>>,
    event_sink: Arc<dyn EventSink>,
}

impl Node {
    /// Creates a stopped node handle for the given config, with core events
    /// going to the log only. The FFI surface uses [`Node::with_event_sink`]
    /// to route them into the persisted public event queue instead.
    pub fn new(config: Config) -> Self {
        let event_sink = Arc::new(LoggingEventSink {
            logger: Arc::new(Logger),
        });
        Self::with_event_sink(config, event_sink)
    }

    /// Creates a stopped node handle whose [`CoreEvent`]s go to `event_sink`
    /// (the U3 event-queue seam).
    pub(crate) fn with_event_sink(config: Config, event_sink: Arc<dyn EventSink>) -> Self {
        Self {
            config,
            state: Mutex::new(None),
            event_sink,
        }
    }

    /// Starts the node: creates the runtime, assembles (fresh or restore),
    /// and spawns the background processor and periodic tasks.
    ///
    /// Fails hard with a typed [`BuildError`] on restore/persistence problems;
    /// tolerates an unreachable Esplora when no channel monitors exist (a
    /// degraded start — see [`Node::is_chain_synced`]).
    pub fn start(&self) -> Result<(), BuildError> {
        let mut state_lock = self.state.lock().unwrap();
        if state_lock.is_some() {
            return Err(BuildError::AlreadyRunning);
        }

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("wallet-core-node")
            .enable_all()
            .build()
            .map_err(|_| BuildError::RuntimeSetupFailed)?;

        let components = build(&self.config, &runtime)?;
        let chain_synced = Arc::new(AtomicBool::new(components.chain_synced_at_start));
        let liquidity_source = Arc::new(LiquiditySource::from_components(
            &components,
            self.config.lsp.clone(),
            self.config.network,
        ));

        let (stop_sender, _) = watch::channel(());
        let (bp_stop_sender, _) = watch::channel(());

        self.spawn_broadcast_task(&runtime, &components, stop_sender.subscribe());
        self.spawn_sync_task(
            &runtime,
            &components,
            stop_sender.subscribe(),
            Arc::clone(&chain_synced),
        );
        self.spawn_peer_reconnect_task(&runtime, &components, stop_sender.subscribe());
        self.spawn_liquidity_event_task(
            &runtime,
            &components,
            Arc::clone(&liquidity_source),
            stop_sender.subscribe(),
        );
        let bp_handle = spawn_background_processor(
            &runtime,
            &components,
            Arc::clone(&liquidity_source),
            Arc::clone(&self.event_sink),
            bp_stop_sender.subscribe(),
        );

        *state_lock = Some(RunningState {
            runtime,
            components,
            liquidity_source,
            stop_sender,
            bp_stop_sender,
            bp_handle,
            chain_synced,
        });
        Ok(())
    }

    /// Requests a JIT invoice for `amount_msat` from the configured LSP,
    /// driving connect → get_info → buy → invoice in one blocking call (call
    /// from a background dispatcher, like `start`). On success the invoice is
    /// ALSO pushed as `InvoiceReady`; every failure is pushed as
    /// `Lsps2Failed` with a distinct reason.
    pub fn receive_jit(&self, amount_msat: u64) -> Result<(String, u64), Lsps2Error> {
        let (liquidity_source, runtime_handle) = {
            let state_lock = self.state.lock().unwrap();
            let state = state_lock.as_ref().ok_or(Lsps2Error::NotRunning)?;
            (
                Arc::clone(&state.liquidity_source),
                state.runtime.handle().clone(),
            )
        };

        // Run the flow on the node runtime and wait outside the state lock,
        // so a concurrent stop() can't deadlock; a dropped runtime surfaces
        // as a closed channel, not a hang.
        let (result_sender, result_receiver) = tokio::sync::oneshot::channel();
        runtime_handle.spawn(async move {
            let _ = result_sender.send(liquidity_source.receive_jit(amount_msat).await);
        });
        let result = result_receiver
            .blocking_recv()
            .unwrap_or(Err(Lsps2Error::Shutdown));

        match result {
            Ok((invoice, expiry_unix_secs)) => {
                let bolt11 = invoice.to_string();
                self.event_sink.emit(CoreEvent::InvoiceReady {
                    bolt11: bolt11.clone(),
                    expiry_unix_secs,
                });
                Ok((bolt11, expiry_unix_secs))
            }
            Err(error) => {
                self.event_sink.emit(CoreEvent::Lsps2Failed {
                    reason: error.to_string(),
                });
                Err(error)
            }
        }
    }

    /// Pays a mainnet BOLT11 invoice (U5). Blocking (route computation): call
    /// from a background dispatcher. Idempotent across restarts: the
    /// `PaymentId` is derived from the payment hash, so LDK rejects a re-send
    /// of an in-flight invoice as a duplicate instead of paying twice.
    ///
    /// The payment outcome arrives via the event queue (`PaymentSuccessful` /
    /// `PaymentFailed`). Failures of the initial attempt (e.g. no route) are
    /// abandoned synchronously by LDK without an event, so they are pushed as
    /// `PaymentFailed` here AND returned as a typed error. Validation
    /// failures and duplicates only return the typed error: nothing was
    /// attempted (or the original attempt still owns the outcome).
    pub fn send_payment(&self, bolt11: &str) -> Result<(), SendError> {
        let channel_manager = {
            let state_lock = self.state.lock().unwrap();
            let state = state_lock.as_ref().ok_or(SendError::NotRunning)?;
            Arc::clone(&state.components.channel_manager)
        };
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("system time before UNIX epoch");

        match send_bolt11(&*channel_manager, bolt11, self.config.network, now) {
            Ok(_payment_id) => Ok(()),
            Err(error) => {
                if error.is_attempt_failure() {
                    self.event_sink.emit(CoreEvent::PaymentFailed {
                        reason: error.to_string(),
                    });
                }
                Err(error)
            }
        }
    }

    /// Test-only: one real `lsps2.get_info` round-trip (the plan's live
    /// Megalith smoke test).
    #[cfg(test)]
    pub(crate) fn lsps2_get_info_live(
        &self,
    ) -> Result<Vec<lightning_liquidity::lsps2::msgs::LSPS2OpeningFeeParams>, Lsps2Error> {
        let (liquidity_source, runtime_handle) = {
            let state_lock = self.state.lock().unwrap();
            let state = state_lock.as_ref().ok_or(Lsps2Error::NotRunning)?;
            (
                Arc::clone(&state.liquidity_source),
                state.runtime.handle().clone(),
            )
        };
        let (result_sender, result_receiver) = tokio::sync::oneshot::channel();
        runtime_handle.spawn(async move {
            let result = async {
                liquidity_source.ensure_lsp_connected().await?;
                liquidity_source.request_opening_params().await
            }
            .await;
            let _ = result_sender.send(result);
        });
        result_receiver
            .blocking_recv()
            .unwrap_or(Err(Lsps2Error::Shutdown))
    }

    /// Stops the node: signals every task, waits for the background processor
    /// (which persists manager/graph/scorer on exit), persists the manager
    /// once more, disconnects peers, and drops the runtime.
    pub fn stop(&self) -> Result<(), BuildError> {
        let state = self
            .state
            .lock()
            .unwrap()
            .take()
            .ok_or(BuildError::NotRunning)?;
        let RunningState {
            runtime,
            components,
            stop_sender,
            bp_stop_sender,
            bp_handle,
            ..
        } = state;

        let _ = stop_sender.send(());
        let _ = bp_stop_sender.send(());
        let _ = runtime
            .block_on(async { tokio::time::timeout(Duration::from_secs(15), bp_handle).await });

        // The background processor already persisted on exit; this is the
        // belt-and-braces write for the paths where it never got to run.
        let persist_res =
            persist_channel_manager(&components.channel_manager, &components.kv_store);
        if let Err(e) = persist_res {
            log_error!(
                components.logger,
                "Failed to persist channel manager on stop: {e}"
            );
        }

        components.peer_manager.disconnect_all_peers();
        runtime.shutdown_timeout(Duration::from_secs(5));
        persist_res
    }

    /// Whether the node is currently running.
    pub fn is_running(&self) -> bool {
        self.state.lock().unwrap().is_some()
    }

    /// The node id, once running.
    pub fn node_id(&self) -> Option<PublicKey> {
        self.state
            .lock()
            .unwrap()
            .as_ref()
            .map(|state| state.components.channel_manager.get_our_node_id())
    }

    /// Whether the last chain sync pass (including the one at start) reached
    /// the tip. `false` while stopped or after a degraded start.
    pub fn is_chain_synced(&self) -> bool {
        self.state
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|state| state.chain_synced.load(Ordering::Acquire))
    }

    /// Total on-chain balance (confirmed + pending) in sats, once running.
    pub fn onchain_balance_sats(&self) -> Option<u64> {
        self.state
            .lock()
            .unwrap()
            .as_ref()
            .map(|state| state.components.onchain_wallet.balance().total().to_sat())
    }

    /// Total lightning balance in msat — the sum of every claimable channel
    /// balance across all monitors — once running.
    pub fn lightning_balance_msat(&self) -> Option<u64> {
        self.state.lock().unwrap().as_ref().map(|state| {
            state
                .components
                .chain_monitor
                .get_claimable_balances(&[])
                .iter()
                .map(|balance| balance.claimable_amount_satoshis() * 1_000)
                .sum()
        })
    }

    fn spawn_broadcast_task(
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

    fn spawn_sync_task(
        &self,
        runtime: &Runtime,
        components: &NodeComponents,
        mut stop_receiver: watch::Receiver<()>,
        chain_synced: Arc<AtomicBool>,
    ) {
        let chain_source = Arc::clone(&components.chain_source);
        let channel_manager = Arc::clone(&components.channel_manager);
        let chain_monitor = Arc::clone(&components.chain_monitor);
        let sweeper = Arc::clone(&components.sweeper);
        let onchain_wallet = Arc::clone(&components.onchain_wallet);
        let gossip_source = Arc::clone(&components.gossip_source);
        let logger = Arc::clone(&components.logger);
        let event_sink = Arc::clone(&self.event_sink);

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
                        let confirmables: Vec<&(dyn Confirm + Sync + Send)> = vec![
                            &*channel_manager,
                            &*chain_monitor,
                            &*sweeper,
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
                    }
                    _ = onchain_interval.tick() => {
                        if let Err(e) = chain_source.sync_onchain_wallet(&onchain_wallet).await {
                            log_error!(logger, "On-chain wallet sync failed: {e}");
                        }
                    }
                    _ = fee_interval.tick() => {
                        if let Err(e) = chain_source.update_fee_rate_estimates().await {
                            log_error!(logger, "Fee rate update failed: {e}");
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

    fn spawn_peer_reconnect_task(
        &self,
        runtime: &Runtime,
        components: &NodeComponents,
        mut stop_receiver: watch::Receiver<()>,
    ) {
        // The LSP peer is always kept connected (U4); LSPS2 requests
        // additionally connect on demand if this loop hasn't run yet.
        let mut peers = self.config.peers.clone();
        peers.push(PeerInfo {
            node_id: self.config.lsp.node_id,
            address: self.config.lsp.address,
        });
        let peer_manager = Arc::clone(&components.peer_manager);
        runtime.spawn(async move {
            let mut interval = tokio::time::interval(PEER_RECONNECT_INTERVAL);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = stop_receiver.changed() => return,
                    _ = interval.tick() => {
                        for peer in &peers {
                            let connected = peer_manager
                                .list_peers()
                                .iter()
                                .any(|details| details.counterparty_node_id == peer.node_id);
                            if !connected {
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
                    }
                }
            }
        });
    }

    /// Pumps `LiquidityManager` events into the [`LiquiditySource`], which
    /// resolves the pending get_info/buy awaits (U4).
    fn spawn_liquidity_event_task(
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

/// Handles LDK events: durable `SpendableOutputs` (U2 fund safety), the U4
/// JIT-receive cluster (0-conf channel acceptance, claimable→claim_funds,
/// claimed→PaymentReceived, channel pending/ready), the U5 send outcomes
/// (PaymentSent/PaymentFailed → the public payment events), and log-and-ack
/// for the rest.
fn handle_ldk_event(
    event: Event,
    sweeper: &Sweeper,
    liquidity_source: &LiquiditySource,
    event_sink: &Arc<dyn EventSink>,
    logger: &Arc<Logger>,
) -> Result<(), ReplayEvent> {
    match event {
        Event::SpendableOutputs {
            outputs,
            channel_id,
        } => {
            // `track_spendable_outputs` persists before returning Ok; on
            // failure we replay the event rather than dropping funds.
            // Static outputs are NOT excluded: our SignerProvider is the bare
            // KeysManager, so the sweeper (not the bdk wallet) owns them.
            sweeper
                .track_spendable_outputs(outputs, channel_id, false, None)
                .map_err(|()| {
                    log_error!(
                        logger,
                        "Failed to persist spendable outputs; replaying event"
                    );
                    ReplayEvent()
                })
        }
        // KTD-9: 0-conf acceptance from the trusted LSP only.
        Event::OpenChannelRequest {
            temporary_channel_id,
            counterparty_node_id,
            ..
        } => {
            liquidity_source.on_open_channel_request(temporary_channel_id, counterparty_node_id);
            Ok(())
        }
        Event::PaymentClaimable {
            payment_hash,
            counterparty_skimmed_fee_msat,
            purpose,
            ..
        } => {
            // claim_funds is idempotent in LDK, so a replayed claimable after
            // an unacked claim is tolerated.
            liquidity_source.on_payment_claimable(
                payment_hash,
                counterparty_skimmed_fee_msat,
                &purpose,
            );
            Ok(())
        }
        // The durable success signal for a receive.
        Event::PaymentClaimed {
            payment_hash,
            amount_msat,
            ..
        } => {
            event_sink.emit(CoreEvent::PaymentReceived {
                amount_msat,
                skimmed_fee_msat: liquidity_source.take_skim(&payment_hash),
            });
            Ok(())
        }
        Event::ChannelPending { .. } => {
            event_sink.emit(CoreEvent::ChannelPending);
            Ok(())
        }
        Event::ChannelReady { .. } => {
            event_sink.emit(CoreEvent::ChannelReady);
            Ok(())
        }
        // The durable success signal for a send (U5).
        Event::PaymentSent {
            payment_hash,
            fee_paid_msat,
            ..
        } => {
            log_info!(
                logger,
                "Outbound payment {payment_hash:?} succeeded (fee paid: {fee_paid_msat:?} msat)"
            );
            event_sink.emit(CoreEvent::PaymentSuccessful);
            Ok(())
        }
        // The terminal failure signal for a send (U5): all retries exhausted
        // or the payment was abandoned.
        Event::PaymentFailed {
            payment_hash,
            reason,
            ..
        } => {
            let reason = describe_failure_reason(reason);
            log_error!(logger, "Outbound payment {payment_hash:?} failed: {reason}");
            event_sink.emit(CoreEvent::PaymentFailed { reason });
            Ok(())
        }
        // Per-path telemetry: the background processor already feeds these to
        // the scorer; the terminal outcome arrives as PaymentSent/Failed.
        Event::PaymentPathSuccessful { payment_hash, .. } => {
            log_info!(logger, "A payment path for {payment_hash:?} succeeded");
            Ok(())
        }
        Event::PaymentPathFailed { payment_hash, .. } => {
            log_info!(
                logger,
                "A payment path for {payment_hash:?} failed; LDK may retry other paths"
            );
            Ok(())
        }
        other => {
            log_info!(logger, "Acking unhandled LDK event: {other:?}");
            Ok(())
        }
    }
}

fn spawn_background_processor(
    runtime: &Runtime,
    components: &NodeComponents,
    liquidity_source: Arc<LiquiditySource>,
    event_sink: Arc<dyn EventSink>,
    bp_stop_receiver: watch::Receiver<()>,
) -> tokio::task::JoinHandle<()> {
    let kv_store = Arc::clone(&components.kv_store);
    let chain_monitor = Arc::clone(&components.chain_monitor);
    let channel_manager = Arc::clone(&components.channel_manager);
    let liquidity_manager = Arc::clone(&components.liquidity_manager);
    let onion_messenger = Arc::clone(&components.onion_messenger);
    let gossip_sync = components.gossip_source.gossip_sync();
    let peer_manager = Arc::clone(&components.peer_manager);
    let sweeper = Arc::clone(&components.sweeper);
    let scorer = Arc::clone(&components.scorer);
    let logger = Arc::clone(&components.logger);
    let error_logger = Arc::clone(&components.logger);

    let event_sweeper = Arc::clone(&components.sweeper);
    let event_logger = Arc::clone(&components.logger);
    let event_handler = move |event: Event| {
        let sweeper = Arc::clone(&event_sweeper);
        let liquidity_source = Arc::clone(&liquidity_source);
        let event_sink = Arc::clone(&event_sink);
        let logger = Arc::clone(&event_logger);
        async move { handle_ldk_event(event, &sweeper, &liquidity_source, &event_sink, &logger) }
    };

    let sleeper = move |duration: Duration| {
        let mut stop = bp_stop_receiver.clone();
        Box::pin(async move {
            tokio::select! {
                _ = stop.changed() => true,
                _ = tokio::time::sleep(duration) => false,
            }
        })
    };

    runtime.spawn(async move {
        let res = process_events_async_with_kv_store_sync(
            kv_store,
            event_handler,
            chain_monitor,
            channel_manager,
            Some(onion_messenger),
            GossipSync::rapid(gossip_sync),
            peer_manager,
            // The LiquidityManager rides in BOTH this slot (message-queue
            // polling + persistence) and the peer manager's custom message
            // handler — omitting either makes LSPS2 silently do nothing.
            Some(liquidity_manager),
            Some(sweeper),
            logger,
            Some(scorer),
            sleeper,
            // KTD-3: mobile platform, interruptible sleeps.
            true,
            || {
                Some(
                    SystemTime::now()
                        .duration_since(SystemTime::UNIX_EPOCH)
                        .expect("system time before UNIX epoch"),
                )
            },
        )
        .await;
        if let Err(e) = res {
            log_error!(error_logger, "Background processor exited with error: {e}");
        }
    })
}
