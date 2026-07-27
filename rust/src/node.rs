//! Node lifecycle (KTD-3, KTD-10): the `Node` owns a 2-worker tokio runtime
//! created at `start()` and dropped at `stop()`. The background processor runs
//! via `process_events_async_with_kv_store_sync` with
//! `mobile_interruptable_platform = true`; periodic chain sync, fee refresh,
//! RGS refresh, broadcast draining, and peer reconnects run as runtime tasks
//! stopped through watch channels.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bitcoin::hashes::Hash as _;
use bitcoin::secp256k1::PublicKey;
use lightning::chain::chaininterface::BroadcasterInterface as _;
use lightning::chain::Confirm;
use lightning::events::{Event, PaymentFailureReason, ReplayEvent};
use lightning::ln::channelmanager::{PaymentId, RecentPaymentDetails};
use lightning::log_error;
use lightning::log_info;
use lightning::types::payment::PaymentHash;
use lightning::util::logger::Logger as _;
use lightning::util::ser::Writeable as _;
use lightning_background_processor::{process_events_async_with_kv_store_sync, GossipSync};
use lightning_persister::fs_store::FilesystemStore;
use tokio::runtime::Runtime;
use tokio::sync::watch;

use crate::builder::{build, persist_channel_manager, BuildError, NodeComponents, KV_STORE_SUBDIR};
use crate::config::{
    Config, FEE_UPDATE_INTERVAL, LIGHTNING_SYNC_INTERVAL, LSPS2_REQUEST_TIMEOUT,
    ONCHAIN_SYNC_INTERVAL, PEER_RECONNECT_INTERVAL, RGS_SYNC_INTERVAL,
};
use crate::history::{
    merge_activity, ActivityRow, CloseRecordSource, NoCloseRecords, PaymentDirection,
    PaymentStatus, PaymentStore, PersistedPayment,
};
use crate::liquidity::{LiquiditySource, Lsps2Error};
use crate::lock::DataDirLock;
use crate::onchain_send::{self, OnchainSendError};
use crate::payment::{
    describe_failure_reason, parse_and_validate, payment_id_for, send_bolt11, SendError,
};
use crate::types::{Logger, Sweeper};
use crate::util::{hex_str, unix_now};

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
    /// An inbound payment was durably claimed (U4/U5). `skimmed_fee_msat` is
    /// the JIT opening fee the LSP withheld, observed on the claimable event.
    PaymentReceived {
        payment_hash: String,
        amount_msat: u64,
        skimmed_fee_msat: Option<u64>,
    },
    /// An inbound (JIT) channel is pending confirmation.
    ChannelPending { channel_id: String },
    /// An inbound (JIT) channel is usable.
    ChannelReady { channel_id: String },
    /// The LSPS2 flow failed (U4).
    Lsps2Failed { reason: String },
    /// An outbound payment succeeded (U5): LDK holds the preimage receipt.
    PaymentSuccessful {
        payment_hash: String,
        fee_paid_msat: Option<u64>,
    },
    /// An outbound payment failed terminally (U5). `reason` is either the
    /// stringified LDK failure reason or the synchronous attempt failure;
    /// `payment_hash` is `None` for BOLT12 payments that failed before an
    /// invoice arrived.
    PaymentFailed {
        payment_hash: Option<String>,
        reason: String,
    },
    /// A cloud-backup (VSS) write is failing; local persistence continues
    /// (U3 fires this from the dual-write store after 10 s of failure, and
    /// from a failed migration batch).
    BackupDegraded { detail: String },
    /// Another client took over this seed's VSS store — the node fenced
    /// itself (U3: durable flag, zero further puts, halt; KTD-3).
    Fenced { detail: String },
    /// The sweep pipeline's state changed (fired by U11).
    #[allow(dead_code)] // Placeholder until U11 fires it.
    SweepStateChanged,
    /// Force-close recovery state changed (fired by U10).
    #[allow(dead_code)] // Placeholder until U10 fires it.
    RecoveryStateChanged,
    /// Restore-from-seed progress (U4): the step strings match the PWA's
    /// Restore.tsx copy exactly.
    RestoreProgress { step: String },
}

/// Consumer of [`CoreEvent`]s (U3 seam).
pub(crate) trait EventSink: Send + Sync {
    fn emit(&self, event: CoreEvent);
}

/// The split on-chain balances (U5): `total_sats` includes untrusted pending;
/// `spendable_sats` is confirmed + trusted pending (bdk's trusted spendable).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OnchainBalances {
    pub total_sats: u64,
    pub spendable_sats: u64,
    pub untrusted_pending_sats: u64,
}

pub(crate) struct LoggingEventSink {
    logger: Arc<Logger>,
}

impl LoggingEventSink {
    /// A log-only sink (tests and the default `Node::new` path).
    pub(crate) fn new() -> Self {
        Self {
            logger: Arc::new(Logger),
        }
    }
}

impl EventSink for LoggingEventSink {
    fn emit(&self, event: CoreEvent) {
        log_info!(self.logger, "Core event: {event:?}");
    }
}

struct RunningState {
    /// Exclusive lock on the storage directory, held for the node's whole
    /// running life and released on `stop()` (or process death). Keeps a second
    /// node — another `Wallet`, another activity, another process — from
    /// diverging channel state on the same seed.
    _data_dir_lock: DataDirLock,
    runtime: Runtime,
    components: NodeComponents,
    liquidity_source: Arc<LiquiditySource>,
    /// Stops the sync/broadcast/reconnect/liquidity tasks.
    stop_sender: watch::Sender<()>,
    /// Stops the background processor (which persists on the way out).
    bp_stop_sender: watch::Sender<()>,
    bp_handle: tokio::task::JoinHandle<()>,
    chain_synced: Arc<AtomicBool>,
    /// U8: while `true` the periodic bdk-wallet sync tick is skipped, so a
    /// sync never races an in-flight on-chain send build (the PWA pauses its
    /// sync loop around `buildSignBroadcast` for the same reason).
    onchain_sync_paused: Arc<AtomicBool>,
    /// U8: wakes an immediate bdk-wallet sync right after a broadcast (the
    /// PWA's `syncNow`), so the spent balance shows without waiting for the
    /// next tick.
    onchain_sync_now: Arc<tokio::sync::Notify>,
}

/// RAII pause for the on-chain sync tick (U8): engaged around a send's
/// build/sign/broadcast, always released — even on an error path.
struct OnchainSyncPause(Arc<AtomicBool>);

impl OnchainSyncPause {
    fn engage(flag: &Arc<AtomicBool>) -> Self {
        flag.store(true, Ordering::Release);
        Self(Arc::clone(flag))
    }
}

impl Drop for OnchainSyncPause {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// The handles a single on-chain send/estimate needs, cloned out of the
/// state lock so the send never holds it (U8).
struct OnchainHandles {
    wallet: Arc<crate::wallet::OnchainWallet>,
    broadcaster: Arc<crate::chain::Broadcaster>,
    /// 6-block rate, ceil'd, clamped >= 2 sat/vB (KTD-9).
    fee_rate_sat_per_vb: u64,
    /// 10,000 sats iff at least one channel is open (R7), read from the
    /// channel manager at call time.
    reserve_sats: u64,
    sync_paused: Arc<AtomicBool>,
    sync_now: Arc<tokio::sync::Notify>,
}

/// A foreground-only mainnet LDK node over the wallet-core stack.
///
/// The only constructor input is a [`Config`]: the mnemonic is auto-generated
/// into the storage dir on first start (U1, R1). Restore-from-words is a
/// separate destructive flow (U4), not a constructor parameter.
pub struct Node {
    config: Config,
    state: Mutex<Option<RunningState>>,
    event_sink: Arc<dyn EventSink>,
    /// The persisted payment history (U5, R11). Like the event queue, it owns
    /// its own store handle, so rows are readable while the node is stopped.
    payment_store: Arc<PaymentStore>,
    /// Close-record read seam for the activity merge — the default empty
    /// source until U10 lands the real store.
    close_records: Arc<dyn CloseRecordSource>,
}

impl Node {
    /// Creates a stopped node handle for the given config, with core events
    /// going to the log only. The FFI surface uses [`Node::with_event_sink`]
    /// to route them into the persisted public event queue instead.
    pub fn new(config: Config) -> Self {
        Self::with_event_sink(config, Arc::new(LoggingEventSink::new()))
    }

    /// Creates a stopped node handle whose [`CoreEvent`]s go to `event_sink`
    /// (the U3 event-queue seam).
    pub(crate) fn with_event_sink(config: Config, event_sink: Arc<dyn EventSink>) -> Self {
        let logger = Arc::new(Logger);
        let kv_store = Arc::new(FilesystemStore::new(
            PathBuf::from(&config.storage_dir).join(KV_STORE_SUBDIR),
        ));
        let payment_store = Arc::new(PaymentStore::new(kv_store, logger));
        Self {
            config,
            state: Mutex::new(None),
            event_sink,
            payment_store,
            close_records: Arc::new(NoCloseRecords),
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

        // U3 (KTD-3): a fenced wallet never starts — another client owns the
        // VSS store; readable queries (history, event queue) stay available
        // while stopped, per KTD-5. Checked before any lock or state touch
        // (build() re-checks defensively).
        if PathBuf::from(&self.config.storage_dir)
            .join(crate::vss::store::FENCED_FLAG_FILE_NAME)
            .exists()
        {
            return Err(BuildError::Fenced);
        }

        // Before building anything: refuse to start if another node already owns
        // this storage directory. Acquired first so a rejected start touches no
        // persisted state.
        std::fs::create_dir_all(&self.config.storage_dir).map_err(|_| BuildError::WriteFailed)?;
        let data_dir_lock = DataDirLock::acquire(std::path::Path::new(&self.config.storage_dir))?;

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("wallet-core-node")
            .enable_all()
            .build()
            .map_err(|_| BuildError::RuntimeSetupFailed)?;

        let components = build(&self.config, &runtime, Arc::clone(&self.event_sink))?;

        // U5 startup reconcile: a pending history row with no LDK
        // counterpart past the grace was a dispatch interrupted by process
        // death — mark it failed ("interrupted") so no phantom in-flight row
        // lives forever. History is informational: a reconcile persist
        // failure is logged, never a failed start.
        {
            let mut live_ids = HashSet::new();
            for details in components.channel_manager.list_recent_payments() {
                match details {
                    RecentPaymentDetails::AwaitingInvoice { payment_id } => {
                        live_ids.insert(hex_str(&payment_id.0));
                    }
                    RecentPaymentDetails::Pending {
                        payment_id,
                        payment_hash,
                        ..
                    } => {
                        live_ids.insert(hex_str(&payment_id.0));
                        live_ids.insert(hex_str(&payment_hash.0));
                    }
                    RecentPaymentDetails::Fulfilled {
                        payment_id,
                        payment_hash,
                    } => {
                        live_ids.insert(hex_str(&payment_id.0));
                        if let Some(payment_hash) = payment_hash {
                            live_ids.insert(hex_str(&payment_hash.0));
                        }
                    }
                    RecentPaymentDetails::Abandoned {
                        payment_id,
                        payment_hash,
                    } => {
                        live_ids.insert(hex_str(&payment_id.0));
                        live_ids.insert(hex_str(&payment_hash.0));
                    }
                }
            }
            if let Err(e) = self
                .payment_store
                .reconcile_pending(&live_ids, unix_now().as_millis() as u64)
            {
                log_error!(
                    components.logger,
                    "Payment-history startup reconcile failed: {e}"
                );
            }
        }

        let chain_synced = Arc::new(AtomicBool::new(components.chain_synced_at_start));
        let liquidity_source = Arc::new(LiquiditySource::from_components(
            &components,
            self.config.lsp.clone(),
            self.config.trusted_lsps.clone(),
            self.config.network,
            LSPS2_REQUEST_TIMEOUT,
        ));

        let (stop_sender, _) = watch::channel(());
        let (bp_stop_sender, _) = watch::channel(());

        // U3 fence watcher: when the store fences itself (divergent-content
        // 409 — another client owns this seed's VSS store), halt the node's
        // tasks and the background processor. The durable flag already blocks
        // the next start; the Fenced event drives the UI.
        {
            let mut fence_rx = components.vss_store.subscribe_fence();
            let stop_tx = stop_sender.clone();
            let bp_stop_tx = bp_stop_sender.clone();
            let logger = Arc::clone(&components.logger);
            runtime.spawn(async move {
                loop {
                    if *fence_rx.borrow() {
                        break;
                    }
                    if fence_rx.changed().await.is_err() {
                        return;
                    }
                }
                log_error!(logger, "Fence tripped: halting node tasks");
                let _ = stop_tx.send(());
                let _ = bp_stop_tx.send(());
            });
        }

        self.spawn_broadcast_task(&runtime, &components, stop_sender.subscribe());
        // U12/KTD-9: rebroadcast any pending transactions persisted by a
        // previous run (crash mid-broadcast must not lose a force-close tx);
        // entries older than 48 h are expired instead. One-shot, failure
        // tolerant — failed entries stay persisted for the next start.
        {
            let chain_source = Arc::clone(&components.chain_source);
            runtime.spawn(async move {
                chain_source
                    .drain_pending_broadcasts(unix_now().as_secs())
                    .await;
            });
        }
        let onchain_sync_paused = Arc::new(AtomicBool::new(false));
        let onchain_sync_now = Arc::new(tokio::sync::Notify::new());
        self.spawn_sync_task(
            &runtime,
            &components,
            stop_sender.subscribe(),
            Arc::clone(&chain_synced),
            Arc::clone(&onchain_sync_paused),
            Arc::clone(&onchain_sync_now),
        );
        self.spawn_peer_reconnect_task(
            &runtime,
            &components,
            Arc::clone(&liquidity_source),
            stop_sender.subscribe(),
        );
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
            Arc::clone(&self.payment_store),
            Arc::clone(&self.event_sink),
            bp_stop_sender.subscribe(),
        );

        *state_lock = Some(RunningState {
            _data_dir_lock: data_dir_lock,
            runtime,
            components,
            liquidity_source,
            stop_sender,
            bp_stop_sender,
            bp_handle,
            chain_synced,
            onchain_sync_paused,
            onchain_sync_now,
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
        let now = unix_now();

        // U5 dispatch writer: the PENDING history row is written after
        // validation and BEFORE the pay attempt, so the row exists for
        // whichever settle follows — the synchronous attempt failure below or
        // a later PaymentSent/PaymentFailed event. Validation failures write
        // nothing (nothing was attempted). History is informational, so a
        // persist failure degrades (logged) instead of blocking the send.
        let invoice = parse_and_validate(bolt11, self.config.network, now)?;
        let payment_id_hex = hex_str(&payment_id_for(&invoice).0);
        let payment_hash_hex = hex_str(invoice.payment_hash().as_byte_array());
        if let Err(e) = self.payment_store.record_pending(
            &payment_id_hex,
            PaymentDirection::Outbound,
            invoice.amount_milli_satoshis().unwrap_or(0),
            now.as_millis() as u64,
        ) {
            log_error!(
                Logger,
                "Failed to write the pending history row for {payment_id_hex}: {e}"
            );
        }

        match send_bolt11(&*channel_manager, bolt11, self.config.network, now) {
            Ok(_payment_id) => Ok(()),
            Err(error) => {
                if error.is_attempt_failure() {
                    // LDK abandoned synchronously without an event: settle the
                    // row and push the public failure ourselves, row first
                    // (the row must never lag the event it explains).
                    if let Err(e) = self.payment_store.settle(
                        &payment_id_hex,
                        PaymentStatus::Failed,
                        None,
                        Some(error.to_string()),
                    ) {
                        log_error!(
                            Logger,
                            "Failed to settle the history row for {payment_id_hex}: {e}"
                        );
                    }
                    self.event_sink.emit(CoreEvent::PaymentFailed {
                        payment_hash: Some(payment_hash_hex),
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
        // Through the dual store: bounded VSS attempt, local write always.
        let persist_res =
            persist_channel_manager(&components.channel_manager, &components.dual_kv_store);
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

    /// Replaces this wallet with the one the entered 12 words back up on VSS
    /// (U4, F3, R1 restore half / R4). Valid ONLY from the stopped state: the
    /// node's state lock is held for the whole flow so no concurrent
    /// `start()` can boot mid-restore, and the engine additionally takes the
    /// data-dir lock against other processes. Blocking (network downloads):
    /// call from a background dispatcher; progress arrives as
    /// `RestoreProgress` events with the PWA's exact step copy.
    ///
    /// Everything before the two-phase write leaves local state untouched
    /// (typed [`RestoreError`]s); once the durable marker is written, any
    /// interruption resumes on the next `start()`.
    pub fn restore(&self, mnemonic: &str) -> Result<(), crate::restore::RestoreError> {
        let state_lock = self.state.lock().unwrap();
        if state_lock.is_some() {
            return Err(crate::restore::RestoreError::NodeRunning);
        }
        crate::restore::run_restore(&self.config, mnemonic, &*self.event_sink, None)?;
        // The store dir was replaced wholesale; drop the in-memory rows the
        // payment store cached from the OLD wallet.
        self.payment_store.reset();
        Ok(())
    }

    /// The stored 12 words for the Backup screen (U4, R1). `None` when no
    /// mnemonic exists yet or the file does not parse.
    pub fn reveal_mnemonic(&self) -> Option<String> {
        let raw = std::fs::read_to_string(
            PathBuf::from(&self.config.storage_dir).join(crate::keys::MNEMONIC_FILE_NAME),
        )
        .ok()?;
        Some(crate::keys::parse_mnemonic(&raw).ok()?.to_string())
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

    /// The split on-chain balances for the U5 FFI surface, once running:
    /// total includes untrusted pending; spendable is bdk's trusted
    /// spendable (confirmed + trusted pending).
    pub fn onchain_balances(&self) -> Option<OnchainBalances> {
        self.state.lock().unwrap().as_ref().map(|state| {
            let balance = state.components.onchain_wallet.balance();
            OnchainBalances {
                total_sats: balance.total().to_sat(),
                spendable_sats: balance.trusted_spendable().to_sat(),
                untrusted_pending_sats: balance.untrusted_pending.to_sat(),
            }
        })
    }

    /// The unified activity feed (U5, KTD-7): payment-store rows (failed
    /// hidden), the bdk wallet's transactions (close-absorbed txids skipped),
    /// and one row per close record, merged and sorted in core — shells never
    /// merge (R14). `None` while stopped (the on-chain arm needs the wallet).
    pub fn list_activity(&self) -> Option<Vec<ActivityRow>> {
        let onchain_txs = self
            .state
            .lock()
            .unwrap()
            .as_ref()
            .map(|state| state.components.onchain_wallet.list_transactions())?;
        Some(merge_activity(
            &self.payment_store.rows(),
            &onchain_txs,
            &self.close_records.summaries(),
        ))
    }

    /// One payment-store row by payment id (U5). Readable while stopped —
    /// the store owns its own KVStore handle, like the event queue.
    pub fn payment_detail(&self, payment_id: &str) -> Option<PersistedPayment> {
        self.payment_store.get(payment_id)
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

    /// Clones the U8 send handles out of the state lock; reserve and fee
    /// rate are read at call time (channel count from the channel manager,
    /// rate from the fee cache).
    fn onchain_handles(&self) -> Result<OnchainHandles, OnchainSendError> {
        let state_lock = self.state.lock().unwrap();
        let state = state_lock.as_ref().ok_or(OnchainSendError::NotRunning)?;
        Ok(OnchainHandles {
            wallet: Arc::clone(&state.components.onchain_wallet),
            broadcaster: Arc::clone(&state.components.broadcaster),
            fee_rate_sat_per_vb: state
                .components
                .chain_source
                .onchain_send_fee_rate_sat_per_vb(),
            reserve_sats: onchain_send::anchor_reserve_sats(
                state.components.channel_manager.list_channels().len(),
            ),
            sync_paused: Arc::clone(&state.onchain_sync_paused),
            sync_now: Arc::clone(&state.onchain_sync_now),
        })
    }

    /// Broadcasts a built-and-signed on-chain send via the persist-first
    /// Broadcaster (U12/KTD-9 sentinels), then wakes the immediate wallet
    /// sync (the PWA's post-broadcast `syncNow`). Returns the txid.
    fn dispatch_onchain_tx(handles: &OnchainHandles, tx: &bitcoin::Transaction) -> String {
        handles.broadcaster.broadcast_transactions(&[tx]);
        handles.sync_now.notify_one();
        tx.compute_txid().to_string()
    }

    /// Fee estimate for an exact-amount on-chain send (U8, R7): builds the
    /// tx at the 6-block rate WITHOUT broadcasting; fees above 50,000 sats
    /// are the typed too-high error (KTD-9).
    pub fn estimate_onchain_fee(
        &self,
        address: &str,
        amount_sats: u64,
    ) -> Result<crate::onchain_send::FeeEstimate, OnchainSendError> {
        let handles = self.onchain_handles()?;
        onchain_send::estimate_fee(
            &handles.wallet,
            self.config.network,
            address,
            amount_sats,
            handles.fee_rate_sat_per_vb,
        )
    }

    /// Max-sendable estimate (U8, R7): drain build minus the anchor reserve
    /// when channels exist; dust floor from the recipient script.
    pub fn estimate_max_sendable(
        &self,
        address: &str,
    ) -> Result<crate::onchain_send::MaxSendEstimate, OnchainSendError> {
        let handles = self.onchain_handles()?;
        onchain_send::estimate_max_sendable(
            &handles.wallet,
            self.config.network,
            address,
            handles.reserve_sats,
            handles.fee_rate_sat_per_vb,
        )
    }

    /// Exact-amount on-chain send (U8, R7): reserve post-check, then the
    /// broadcast-boundary drift + fee guards, then the persist-first
    /// broadcast; sync is paused around the build and `sync_now` follows the
    /// dispatch. `expected_*` are the review-screen values (R5 drift guard).
    pub fn send_onchain(
        &self,
        address: &str,
        amount_sats: u64,
        expected_amount_sats: u64,
        expected_fee_sats: u64,
    ) -> Result<String, OnchainSendError> {
        let handles = self.onchain_handles()?;
        let expected = onchain_send::DriftGuard::for_address(
            address,
            self.config.network,
            expected_amount_sats,
            expected_fee_sats,
        )?;
        let _pause = OnchainSyncPause::engage(&handles.sync_paused);
        let tx = onchain_send::send_to_address(
            &handles.wallet,
            self.config.network,
            address,
            amount_sats,
            &expected,
            handles.reserve_sats,
            handles.fee_rate_sat_per_vb,
        )?;
        Ok(Self::dispatch_onchain_tx(&handles, &tx))
    }

    /// On-chain send-max (U8, AE6): drains fully at zero channels; with
    /// channels the built tx leaves exactly 10,000 sats as an explicit
    /// reserve output to an internal address. Same drift guard, pause, and
    /// persist-first broadcast as [`Node::send_onchain`].
    pub fn send_onchain_max(
        &self,
        address: &str,
        expected_amount_sats: u64,
        expected_fee_sats: u64,
    ) -> Result<String, OnchainSendError> {
        let handles = self.onchain_handles()?;
        let expected = onchain_send::DriftGuard::for_address(
            address,
            self.config.network,
            expected_amount_sats,
            expected_fee_sats,
        )?;
        let _pause = OnchainSyncPause::engage(&handles.sync_paused);
        let tx = onchain_send::send_max(
            &handles.wallet,
            self.config.network,
            address,
            &expected,
            handles.reserve_sats,
            handles.fee_rate_sat_per_vb,
        )?;
        Ok(Self::dispatch_onchain_tx(&handles, &tx))
    }

    /// Next unused receive address on the external keychain (U8): the
    /// changeset is persisted after the reveal, so a restart keeps the index.
    pub fn next_receive_address(&self) -> Result<String, OnchainSendError> {
        let wallet = {
            let state_lock = self.state.lock().unwrap();
            let state = state_lock.as_ref().ok_or(OnchainSendError::NotRunning)?;
            Arc::clone(&state.components.onchain_wallet)
        };
        wallet
            .next_receive_address()
            .map_err(|()| OnchainSendError::BuildFailed {
                detail: "failed to persist the address reveal".to_string(),
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
        onchain_sync_paused: Arc<AtomicBool>,
        onchain_sync_now: Arc<tokio::sync::Notify>,
    ) {
        let chain_source = Arc::clone(&components.chain_source);
        let channel_manager = Arc::clone(&components.channel_manager);
        let chain_monitor = Arc::clone(&components.chain_monitor);
        let sweeper = Arc::clone(&components.sweeper);
        let onchain_wallet = Arc::clone(&components.onchain_wallet);
        let gossip_source = Arc::clone(&components.gossip_source);
        let dual_kv_store = Arc::clone(&components.dual_kv_store);
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
                        // U3 (KTD-3 CM semantics): a channel-manager write
                        // whose bounded VSS attempt failed left a dirty flag;
                        // this tick retries it without ever blocking the
                        // background processor.
                        if dual_kv_store.cm_dirty() {
                            dual_kv_store.retry_cm(channel_manager.encode()).await;
                        }
                    }
                    _ = onchain_interval.tick() => {
                        // U8: skipped while a send is building/signing so the
                        // sync never steps on the wallet mid-send.
                        if onchain_sync_paused.load(Ordering::Acquire) {
                            continue;
                        }
                        if let Err(e) = chain_source.sync_onchain_wallet(&onchain_wallet).await {
                            log_error!(logger, "On-chain wallet sync failed: {e}");
                        }
                    }
                    _ = onchain_sync_now.notified() => {
                        // U8: the post-broadcast immediate sync (PWA syncNow)
                        // — runs regardless of the pause flag, exactly like
                        // the PWA's in-window syncNow.
                        if let Err(e) = chain_source.sync_onchain_wallet(&onchain_wallet).await {
                            log_error!(logger, "Post-broadcast wallet sync failed: {e}");
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

    fn spawn_peer_reconnect_task(
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

/// Settles the history row for a `PaymentSent` and emits the public success —
/// row FIRST (U5 persist-then-ack): if the settle cannot be made durable the
/// event is replayed and nothing is emitted, so a consumer never sees a
/// success the store doesn't know about. The row key is LDK's `payment_id`
/// when present, else the payment hash (PWA `event-handler.ts:297-300`);
/// replays are absorbed by the store's idempotent settle.
pub(crate) fn settle_payment_sent(
    payment_store: &PaymentStore,
    event_sink: &dyn EventSink,
    logger: &Arc<Logger>,
    payment_id: Option<PaymentId>,
    payment_hash: PaymentHash,
    fee_paid_msat: Option<u64>,
) -> Result<(), ReplayEvent> {
    let row_key = payment_id
        .map(|id| hex_str(&id.0))
        .unwrap_or_else(|| hex_str(&payment_hash.0));
    payment_store
        .settle(&row_key, PaymentStatus::Succeeded, fee_paid_msat, None)
        .map_err(|e| {
            log_error!(logger, "Deferring PaymentSent {row_key}: {e}");
            ReplayEvent()
        })?;
    log_info!(
        logger,
        "Outbound payment {row_key} succeeded (fee paid: {fee_paid_msat:?} msat)"
    );
    event_sink.emit(CoreEvent::PaymentSuccessful {
        payment_hash: hex_str(&payment_hash.0),
        fee_paid_msat,
    });
    Ok(())
}

/// Settles the history row for a terminal `PaymentFailed` and emits the
/// public failure — row first, exactly like [`settle_payment_sent`].
pub(crate) fn settle_payment_failed(
    payment_store: &PaymentStore,
    event_sink: &dyn EventSink,
    logger: &Arc<Logger>,
    payment_id: PaymentId,
    payment_hash: Option<PaymentHash>,
    reason: Option<PaymentFailureReason>,
) -> Result<(), ReplayEvent> {
    let row_key = hex_str(&payment_id.0);
    let reason = describe_failure_reason(reason);
    payment_store
        .settle(&row_key, PaymentStatus::Failed, None, Some(reason.clone()))
        .map_err(|e| {
            log_error!(logger, "Deferring PaymentFailed {row_key}: {e}");
            ReplayEvent()
        })?;
    log_error!(logger, "Outbound payment {row_key} failed: {reason}");
    event_sink.emit(CoreEvent::PaymentFailed {
        payment_hash: payment_hash.map(|hash| hex_str(&hash.0)),
        reason,
    });
    Ok(())
}

/// Writes the inbound SUCCEEDED row for a `PaymentClaimed` and emits the
/// public receive — row first. `take_skim` is consumed only after the row is
/// durable, so a replayed claim never loses the skim to a failed persist.
pub(crate) fn record_payment_claimed(
    payment_store: &PaymentStore,
    event_sink: &dyn EventSink,
    logger: &Arc<Logger>,
    payment_hash: PaymentHash,
    amount_msat: u64,
    now_ms: u64,
    take_skim: impl FnOnce() -> Option<u64>,
) -> Result<(), ReplayEvent> {
    let hash_hex = hex_str(&payment_hash.0);
    payment_store
        .record_claimed(&hash_hex, amount_msat, now_ms)
        .map_err(|e| {
            log_error!(logger, "Deferring PaymentClaimed {hash_hex}: {e}");
            ReplayEvent()
        })?;
    event_sink.emit(CoreEvent::PaymentReceived {
        payment_hash: hash_hex,
        amount_msat,
        skimmed_fee_msat: take_skim(),
    });
    Ok(())
}

/// Handles LDK events: durable `SpendableOutputs` (U2 fund safety), the U4
/// JIT-receive cluster (0-conf channel acceptance, claimable→claim_funds,
/// claimed→PaymentReceived, channel pending/ready), the U5 payment outcomes
/// (PaymentSent/PaymentFailed/PaymentClaimed → history row settled durably,
/// THEN the public event — persist-then-ack), and log-and-ack for the rest.
fn handle_ldk_event(
    event: Event,
    sweeper: &Sweeper,
    liquidity_source: &LiquiditySource,
    payment_store: &PaymentStore,
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
            // Static outputs are NOT excluded: the signer's payment keys are
            // still KeysManager-derived (U1's provider only redirects
            // destination/shutdown scripts), so the sweeper (not the bdk
            // wallet) owns them.
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
        // The durable success signal for a receive: history row, then event.
        Event::PaymentClaimed {
            payment_hash,
            amount_msat,
            ..
        } => record_payment_claimed(
            payment_store,
            &**event_sink,
            logger,
            payment_hash,
            amount_msat,
            unix_now().as_millis() as u64,
            || liquidity_source.take_skim(&payment_hash),
        ),
        Event::ChannelPending { channel_id, .. } => {
            event_sink.emit(CoreEvent::ChannelPending {
                channel_id: hex_str(&channel_id.0),
            });
            Ok(())
        }
        Event::ChannelReady { channel_id, .. } => {
            event_sink.emit(CoreEvent::ChannelReady {
                channel_id: hex_str(&channel_id.0),
            });
            Ok(())
        }
        // The durable success signal for a send (U5): row, then event.
        Event::PaymentSent {
            payment_id,
            payment_hash,
            fee_paid_msat,
            ..
        } => settle_payment_sent(
            payment_store,
            &**event_sink,
            logger,
            payment_id,
            payment_hash,
            fee_paid_msat,
        ),
        // The terminal failure signal for a send (U5): all retries exhausted
        // or the payment was abandoned. Row first, then event.
        Event::PaymentFailed {
            payment_id,
            payment_hash,
            reason,
        } => settle_payment_failed(
            payment_store,
            &**event_sink,
            logger,
            payment_id,
            payment_hash,
            reason,
        ),
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
    payment_store: Arc<PaymentStore>,
    event_sink: Arc<dyn EventSink>,
    bp_stop_receiver: watch::Receiver<()>,
) -> tokio::task::JoinHandle<()> {
    // U3: the background processor persists through the dual-write store —
    // channel-manager writes get the bounded VSS-then-local treatment,
    // graph/scorer/sweeper/liquidity stay local-only. Monitors do NOT come
    // through here (they use the ChainMonitor's custom async `Persist`).
    let kv_store = Arc::clone(&components.dual_kv_store);
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
        let payment_store = Arc::clone(&payment_store);
        let event_sink = Arc::clone(&event_sink);
        let logger = Arc::clone(&event_logger);
        async move {
            handle_ldk_event(
                event,
                &sweeper,
                &liquidity_source,
                &payment_store,
                &event_sink,
                &logger,
            )
        }
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
            || Some(unix_now()),
        )
        .await;
        if let Err(e) = res {
            log_error!(error_logger, "Background processor exited with error: {e}");
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    use bitcoin::hashes::sha256;
    use bitcoin::secp256k1::{Secp256k1, SecretKey};
    use lightning::types::payment::PaymentSecret;
    use lightning_invoice::{Bolt11Invoice, Currency, InvoiceBuilder};

    use crate::events::EventQueue;
    use crate::history::{ActivityStatus, PAYMENT_HISTORY_PRIMARY_NAMESPACE};

    #[derive(Default)]
    struct CapturingSink(Mutex<Vec<CoreEvent>>);

    impl EventSink for CapturingSink {
        fn emit(&self, event: CoreEvent) {
            self.0.lock().unwrap().push(event);
        }
    }

    fn store_in(dir: &Path) -> PaymentStore {
        PaymentStore::new(
            Arc::new(FilesystemStore::new(dir.join("store"))),
            Arc::new(Logger),
        )
    }

    fn offline_config(dir: &Path) -> Config {
        let mut config = Config::new(dir.to_str().unwrap().to_string());
        config.esplora_url = "http://127.0.0.1:1".to_string();
        config.rgs_url = "http://127.0.0.1:1/snapshot".to_string();
        // Offline suites run local-only; the VSS-enabled paths are covered by
        // the U3 tests with an injected mock transport.
        config.vss_disabled = true;
        config
    }

    fn payment_hash(byte: u8) -> PaymentHash {
        PaymentHash([byte; 32])
    }

    /// U5 persist-then-ack, failure half: when the settle CANNOT be made
    /// durable, the handler asks LDK to REPLAY the event and emits NOTHING —
    /// the public event queue never runs ahead of the history store.
    #[cfg(unix)]
    #[test]
    fn payment_settles_persist_before_any_public_event_is_emitted() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        let sink = CapturingSink::default();
        let logger = Arc::new(Logger);
        store
            .record_pending(&"77".repeat(32), PaymentDirection::Outbound, 1_000, 1)
            .unwrap();

        let namespace_dir = dir
            .path()
            .join("store")
            .join(PAYMENT_HISTORY_PRIMARY_NAMESPACE);
        let writable = std::fs::metadata(&namespace_dir).unwrap().permissions();
        std::fs::set_permissions(&namespace_dir, std::fs::Permissions::from_mode(0o555)).unwrap();
        let result = settle_payment_sent(
            &store,
            &sink,
            &logger,
            Some(PaymentId([0x77; 32])),
            payment_hash(0x77),
            Some(21),
        );
        std::fs::set_permissions(&namespace_dir, writable).unwrap();

        assert!(
            result.is_err(),
            "a non-durable settle must request a replay"
        );
        assert!(
            sink.0.lock().unwrap().is_empty(),
            "no public event may be emitted before the settle is durable"
        );

        // The replay settles and emits once persistence recovers.
        settle_payment_sent(
            &store,
            &sink,
            &logger,
            Some(PaymentId([0x77; 32])),
            payment_hash(0x77),
            Some(21),
        )
        .unwrap();
        assert_eq!(
            store.get(&"77".repeat(32)).unwrap().status,
            PaymentStatus::Succeeded
        );
        assert_eq!(
            sink.0.lock().unwrap().clone(),
            vec![CoreEvent::PaymentSuccessful {
                payment_hash: "77".repeat(32),
                fee_paid_msat: Some(21),
            }]
        );
    }

    /// The crash-between-persist-and-ack window (U5): the settle is durable,
    /// the public event is queued but NEVER acked, the process dies. On
    /// rebuild the queue redelivers the event AND LDK replays PaymentSent —
    /// the replayed settle is a no-op, so the row settles exactly once.
    #[test]
    fn replayed_payment_sent_after_crash_before_ack_settles_exactly_once() {
        let dir = tempfile::tempdir().unwrap();
        let logger = Arc::new(Logger);
        let kv = || Arc::new(FilesystemStore::new(dir.path().join("store")));

        {
            let store = PaymentStore::new(kv(), Arc::clone(&logger));
            let queue = EventQueue::new(kv(), Arc::clone(&logger));
            store
                .record_pending(&"88".repeat(32), PaymentDirection::Outbound, 5_000, 1)
                .unwrap();
            settle_payment_sent(
                &store,
                &queue,
                &logger,
                Some(PaymentId([0x88; 32])),
                payment_hash(0x88),
                Some(7),
            )
            .unwrap();
            // Crash here: the event stays in the persisted queue, unacked.
        }

        let store = PaymentStore::new(kv(), Arc::clone(&logger));
        let queue = EventQueue::new(kv(), Arc::clone(&logger));
        assert_eq!(
            store.get(&"88".repeat(32)).unwrap().status,
            PaymentStatus::Succeeded,
            "the settle was durable before the crash"
        );
        // LDK replays the unhandled event on restart; the settle is a no-op
        // and the fee recorded by the first delivery survives.
        settle_payment_sent(
            &store,
            &queue,
            &logger,
            Some(PaymentId([0x88; 32])),
            payment_hash(0x88),
            Some(999),
        )
        .unwrap();
        let row = store.get(&"88".repeat(32)).unwrap();
        assert_eq!(row.status, PaymentStatus::Succeeded);
        assert_eq!(
            row.fee_paid_msat,
            Some(7),
            "exactly-once: replay changes nothing"
        );
        // The unacked public event was redelivered from disk; the idempotent
        // consumer handles the duplicate emit (handle-then-ack contract).
        assert_eq!(
            queue.ack(),
            Some(crate::events::Event::PaymentSuccessful {
                payment_hash: "88".repeat(32),
                fee_paid_msat: Some(7),
            })
        );
    }

    /// A replayed PaymentClaimed after a crash-before-ack never duplicates
    /// the inbound row, and the row is durable before the event.
    #[test]
    fn replayed_payment_claimed_never_duplicates_the_inbound_row() {
        let dir = tempfile::tempdir().unwrap();
        let logger = Arc::new(Logger);
        let store = store_in(dir.path());
        let sink = CapturingSink::default();

        record_payment_claimed(
            &store,
            &sink,
            &logger,
            payment_hash(0x99),
            250_000,
            1_000,
            || Some(2_000),
        )
        .unwrap();
        // Replay: the skim was consumed by the first delivery (None now).
        record_payment_claimed(
            &store,
            &sink,
            &logger,
            payment_hash(0x99),
            250_000,
            2_000,
            || None,
        )
        .unwrap();

        assert_eq!(store.rows().len(), 1, "re-claiming must not duplicate");
        let row = store.get(&"99".repeat(32)).unwrap();
        assert_eq!(row.direction, PaymentDirection::Inbound);
        assert_eq!(row.status, PaymentStatus::Succeeded);
        assert_eq!(row.created_at_ms, 1_000, "first claim's facts win");
    }

    /// U5 startup reconcile at the Node level: a stale orphaned pending row
    /// (no LDK counterpart — the fresh channel manager knows no payments) is
    /// failed as "interrupted" by start(); a young orphan stays pending.
    #[test]
    fn start_reconciles_orphaned_pending_rows_to_interrupted() {
        let dir = tempfile::tempdir().unwrap();
        let stale = "ab".repeat(32);
        let young = "cd".repeat(32);
        {
            let store = store_in(dir.path());
            store
                .record_pending(&stale, PaymentDirection::Outbound, 1_000, 1_000)
                .unwrap();
            store
                .record_pending(
                    &young,
                    PaymentDirection::Outbound,
                    2_000,
                    unix_now().as_millis() as u64,
                )
                .unwrap();
        }

        let node = Node::new(offline_config(dir.path()));
        node.start().expect("offline degraded start");

        let stale_row = node.payment_detail(&stale).unwrap();
        assert_eq!(stale_row.status, PaymentStatus::Failed);
        assert_eq!(
            stale_row.failure_reason.as_deref(),
            Some(crate::history::INTERRUPTED_REASON)
        );
        assert_eq!(
            node.payment_detail(&young).unwrap().status,
            PaymentStatus::Pending,
            "a just-dispatched row must survive the reconcile"
        );
        node.stop().unwrap();
    }

    /// U8 at the Node seam: every on-chain endpoint is NotRunning while
    /// stopped; once started (offline, degraded), the receive path serves a
    /// mainnet address and persists the reveal across a restart, and an
    /// empty wallet's estimates fail typed, never panic.
    #[test]
    fn onchain_endpoints_follow_the_node_lifecycle() {
        let dir = tempfile::tempdir().unwrap();
        let node = Node::new(offline_config(dir.path()));
        const ADDR: &str = "bc1qw508d6qejxtdg4y5r3zarvary0c5xw7kv8f3t4";

        assert_eq!(
            node.next_receive_address().unwrap_err(),
            OnchainSendError::NotRunning
        );
        assert_eq!(
            node.estimate_onchain_fee(ADDR, 10_000).unwrap_err(),
            OnchainSendError::NotRunning
        );
        assert_eq!(
            node.send_onchain(ADDR, 10_000, 10_000, 100).unwrap_err(),
            OnchainSendError::NotRunning
        );
        assert_eq!(
            node.send_onchain_max(ADDR, 10_000, 100).unwrap_err(),
            OnchainSendError::NotRunning
        );

        node.start().expect("offline degraded start");
        let address = node.next_receive_address().unwrap();
        assert!(address.starts_with("bc1q"), "BIP84 mainnet address");
        // Zero channels: the reserve is inactive, so an empty wallet's max
        // estimate fails on the balance, not the reserve (R7).
        assert_eq!(
            node.estimate_max_sendable(ADDR).unwrap_err(),
            OnchainSendError::BalanceTooLow
        );
        assert!(matches!(
            node.estimate_onchain_fee(ADDR, 10_000).unwrap_err(),
            OnchainSendError::BuildFailed { .. }
        ));
        node.stop().unwrap();

        // The reveal survives the restart (address-reveal learning).
        node.start().expect("offline degraded restart");
        assert_eq!(node.next_receive_address().unwrap(), address);
        node.stop().unwrap();
    }

    fn signed_mainnet_invoice() -> Bolt11Invoice {
        let secret = SecretKey::from_slice(&[0x4d; 32]).unwrap();
        InvoiceBuilder::new(Currency::Bitcoin)
            .description("u5 dispatch test".to_string())
            .payment_hash(sha256::Hash::from_byte_array([0x42; 32]))
            .payment_secret(PaymentSecret([0x24; 32]))
            .duration_since_epoch(unix_now())
            .min_final_cltv_expiry_delta(144)
            .expiry_time(Duration::from_secs(3_600))
            .amount_milli_satoshis(50_000_000)
            .build_signed(|hash| Secp256k1::new().sign_ecdsa_recoverable(hash, &secret))
            .unwrap()
    }

    /// The wired dispatch (U5): send_payment writes the pending row keyed by
    /// the derived payment id, and a synchronous attempt failure settles it
    /// FAILED with the same reason the public event carries. The failed row
    /// is hidden from the activity feed but visible via payment_detail.
    #[test]
    fn send_payment_writes_and_settles_the_history_row() {
        let dir = tempfile::tempdir().unwrap();
        let sink = Arc::new(CapturingSink::default());
        let node = Node::with_event_sink(offline_config(dir.path()), Arc::clone(&sink) as _);
        node.start().expect("offline degraded start");

        let invoice = signed_mainnet_invoice();
        let payment_id_hex = "42".repeat(32); // bolt11: payment id == hash
        assert_eq!(
            node.send_payment(&invoice.to_string()),
            Err(SendError::RouteNotFound)
        );

        let row = node
            .payment_detail(&payment_id_hex)
            .expect("dispatch must write a history row");
        assert_eq!(row.direction, PaymentDirection::Outbound);
        assert_eq!(row.amount_msat, 50_000_000);
        assert_eq!(row.status, PaymentStatus::Failed);
        assert_eq!(
            row.failure_reason.as_deref(),
            Some(SendError::RouteNotFound.to_string().as_str()),
            "the row and the public event carry the same reason"
        );

        // Validation failures never touch the store.
        assert!(matches!(
            node.send_payment("junk"),
            Err(SendError::InvalidInvoice(_))
        ));
        let feed = node.list_activity().unwrap();
        assert!(
            feed.iter().all(|r| r.id != payment_id_hex),
            "failed rows are hidden from the feed (KTD-7)"
        );
        assert!(
            feed.iter().all(|r| r.status != ActivityStatus::Failed),
            "the feed never exposes a failed status"
        );
        node.stop().unwrap();

        // Rows stay readable while stopped; the feed needs the node.
        assert!(node.payment_detail(&payment_id_hex).is_some());
        assert!(node.list_activity().is_none());
    }
}
