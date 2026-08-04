//! Node lifecycle (KTD-3, KTD-10): the `Node` owns a 2-worker tokio runtime
//! created at `start()` and dropped at `stop()`. The background processor runs
//! via `process_events_async_with_kv_store_sync` with
//! `mobile_interruptable_platform = true`; periodic chain sync, fee refresh,
//! RGS refresh, broadcast draining, and peer reconnects run as runtime tasks
//! stopped through watch channels.
//!
//! This file keeps the lifecycle core — `new`/`start`/`stop`/`restore` and the
//! read-only queries — while the feature surfaces live in sibling modules under
//! `node/`. `impl Node` is split across them; multiple inherent `impl` blocks
//! are legal anywhere in the defining crate, and a child module can see its
//! parent's private items, so the split needs no visibility widening outside
//! the test helpers below.

mod channels_api;
mod event_handler;

use event_handler::spawn_background_processor;
mod onchain;
mod payments;
mod tasks;

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bitcoin::secp256k1::PublicKey;
use lightning::ln::channelmanager::RecentPaymentDetails;
use lightning::log_error;
use lightning::log_info;
use lightning::util::logger::Logger as _;
use lightning_persister::fs_store::FilesystemStore;
use tokio::runtime::Runtime;
use tokio::sync::watch;

use crate::builder::{build, persist_channel_manager, BuildError, NodeComponents, KV_STORE_SUBDIR};
use crate::close_records::{CloseRecord, CloseRecordStore};
use crate::config::{
    Config, FEE_UPDATE_INTERVAL, LIGHTNING_SYNC_INTERVAL, LSPS2_REQUEST_TIMEOUT,
    ONCHAIN_SYNC_INTERVAL, PEER_RECONNECT_INTERVAL, RGS_SYNC_INTERVAL,
};
use crate::history::{
    merge_activity, ActivityRow, CloseRecordSource, PaymentStore, PersistedPayment,
};
use crate::liquidity::LiquiditySource;
use crate::lock::DataDirLock;
use crate::onchain_send;
use crate::recovery::{RecoveryState, RecoveryStore, RecoverySweeper};
use crate::sweep::{PendingSweepInfo, SweepBroadcast, SweepEngine, SweepStore};
use crate::types::Logger;
use crate::util::{hex_str, now_ms, unix_now};

/// Internal core events, mapped into the public FFI `Event` enum by the
/// persisted event queue (the [`EventSink`] seam).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CoreEvent {
    /// A background chain sync pass reached the tip.
    ChainSyncCompleted,
    /// A background chain sync pass failed; it will be retried.
    ChainSyncFailed,
    /// A JIT invoice is ready to display (U4/U7). `expiry_unix_secs` is the
    /// invoice's clamped expiry (R6: the quote's `valid_until` minus the
    /// 30 s flight margin, capped at 3600 s) as UNIX seconds.
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
    /// A channel closed (U9): `reason` is LDK's `ClosureReason` rendered.
    ChannelClosed { channel_id: String, reason: String },
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
    /// The sweep pipeline's state changed (U11): pending outputs, a failed
    /// attempt, or the shortfall/add-funds state — consumers re-read
    /// `pending_sweep()`.
    SweepStateChanged,
    /// Force-close recovery state changed (U10). Payload-less by design
    /// (PWA parity): consumers re-read `recovery_state()` — a stale payload
    /// resolving late would show yesterday's state.
    RecoveryStateChanged,
    /// An on-chain (bdk) sync pass changed wallet-visible data (U8): a new
    /// transaction, a confirmation, or a mempool eviction. Payload-less like
    /// its siblings above — consumers re-read `balances()` / activity.
    ///
    /// WHY IT EXISTS: the on-chain sync tick used to persist its changeset and
    /// emit nothing, and both shells re-query wallet data only on events. A
    /// recovered sweep landed in the persisted bdk changeset while the UI sat
    /// on a stale balance for minutes; a relaunch showed the right number
    /// immediately. Emitted only when something actually changed, so a quiet
    /// wallet stays silent across every 120 s tick.
    OnchainStateChanged,
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
    /// U10 close records: local-first store + best-effort VSS singleton
    /// (attached while running); the activity merge's close arm.
    close_records: Arc<CloseRecordStore>,
    /// U10 recovery state machine: local-first + best-effort VSS blob.
    recovery: Arc<RecoveryStore>,
    /// U11 spendable-outputs store (KTD-8): owns its own store handle so
    /// `pending_sweep()` is readable while stopped; the running
    /// [`SweepEngine`] shares it.
    sweep_store: Arc<SweepStore>,
}

/// Runs `fut` on the node runtime and blocks the calling (dispatcher) thread
/// for the result — the receive_jit pattern: spawn on the node runtime, wait
/// outside the state lock, so a concurrent `stop()` can't deadlock. `None`
/// when the runtime shut down before replying (dropped sender), never a hang;
/// each caller supplies its own shutdown fallback.
pub(super) fn spawn_and_wait<T: Send + 'static>(
    handle: &tokio::runtime::Handle,
    fut: impl std::future::Future<Output = T> + Send + 'static,
) -> Option<T> {
    let (result_sender, result_receiver) = tokio::sync::oneshot::channel();
    handle.spawn(async move {
        let _ = result_sender.send(fut.await);
    });
    result_receiver.blocking_recv().ok()
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
        let payment_store = Arc::new(PaymentStore::new(
            Arc::clone(&kv_store),
            Arc::clone(&logger),
        ));
        // U10: both stores own their local handles (readable while stopped);
        // the VSS halves attach at start().
        let close_records = Arc::new(CloseRecordStore::new(
            Arc::clone(&kv_store),
            Arc::clone(&logger),
        ));
        let recovery = Arc::new(RecoveryStore::new(
            Arc::clone(&kv_store),
            Arc::clone(&event_sink),
            Arc::clone(&logger),
        ));
        let sweep_store = Arc::new(SweepStore::new(kv_store, logger));
        Self {
            config,
            state: Mutex::new(None),
            event_sink,
            payment_store,
            close_records,
            recovery,
            sweep_store,
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
            if let Err(e) = self.payment_store.reconcile_pending(&live_ids, now_ms()) {
                log_error!(
                    components.logger,
                    "Payment-history startup reconcile failed: {e}"
                );
            }
        }

        // U10: attach the VSS halves for the running session. The remote seed
        // (and the missed-descriptor replay that depends on it) is spawned
        // further down, once the sweep engine exists.
        self.close_records
            .attach_vss(Arc::clone(&components.vss_store));
        self.recovery.attach_vss(Arc::clone(&components.vss_store));

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

        // Async payments, recipient half (U3). A no-op for the default empty
        // configuration, which is every shipped build. Never fails start:
        // async receive is an opt-in extra, and the standard receive paths
        // must not depend on it.
        match crate::receive::apply_static_invoice_server_paths(
            &components.channel_manager,
            &self.config.static_invoice_server_paths,
        ) {
            Ok(0) => {}
            Ok(count) => log_info!(
                components.logger,
                "Async receive: configured {count} path(s) to the static invoice server"
            ),
            Err(()) => log_error!(
                components.logger,
                "Async receive: LDK rejected the configured static invoice server paths"
            ),
        }

        // U11 (KTD-8): the core-owned sweep engine over the shared
        // descriptor store. The reserve closure reads the channel count at
        // sweep time (U8 arithmetic: 10,000 sats iff any channel is open).
        let sweep_engine = {
            let channel_manager = Arc::clone(&components.channel_manager);
            Arc::new(SweepEngine::new(
                Arc::clone(&self.sweep_store),
                Arc::clone(&components.keys_manager),
                Arc::clone(&components.onchain_wallet),
                Arc::clone(&components.chain_source) as Arc<dyn SweepBroadcast>,
                components.chain_source.fee_estimator(),
                Arc::clone(&self.close_records),
                Arc::new(move || {
                    onchain_send::anchor_reserve_sats(channel_manager.list_channels().len())
                }),
                Arc::clone(&self.event_sink),
                Arc::clone(&components.logger),
            ))
        };
        let sweep_wake = Arc::new(tokio::sync::Notify::new());
        self.spawn_sweep_task(
            &runtime,
            Arc::clone(&sweep_engine),
            Arc::clone(&sweep_wake),
            stop_sender.subscribe(),
        );
        // U10: seed both singletons from the remote (pull cross-device state
        // into empty local stores; merges are idempotent so a late seed is
        // safe), THEN run the once-per-boot missed-descriptor replay.
        //
        // The ordering is load-bearing in both directions. The replay is
        // close-record-driven, and on a cross-client restore the records may
        // arrive only with this VSS seed — so it must run after it. And it
        // needs monitors already synced to tip, which `build()` did
        // synchronously before `watch_channel` (`get_spendable_outputs` counts
        // confirmations against the monitor's `best_block`) — so it cannot run
        // any earlier than start. One shot per boot: restore, silent recovery,
        // and plain restart all benefit, and a locally-created wallet that
        // missed an event benefits identically. See `crate::replay`.
        {
            let vss = Arc::clone(&components.vss_store);
            let close_records = Arc::clone(&self.close_records);
            let recovery = Arc::clone(&self.recovery);
            let chain_monitor = Arc::clone(&components.chain_monitor);
            let chain_source = Arc::clone(&components.chain_source);
            let logger = Arc::clone(&components.logger);
            let engine = Arc::clone(&sweep_engine);
            let wake = Arc::clone(&sweep_wake);
            // Only a start whose initial chain sync SUCCEEDED may replay: a
            // degraded start is only reachable with zero monitors (see
            // `build()`), so this costs nothing real — it just keeps the
            // "monitors are at tip" precondition explicit rather than implied.
            let synced_at_start = components.chain_synced_at_start;
            runtime.spawn(async move {
                if let Some((bytes, _)) = vss
                    .fetch_versioned(crate::vss::store::CLOSE_RECORDS_VSS_KEY)
                    .await
                {
                    close_records.seed_from_remote(&bytes);
                }
                if let Some((bytes, _)) = vss
                    .fetch_versioned(crate::vss::store::FORCE_CLOSE_RECOVERY_VSS_KEY)
                    .await
                {
                    recovery.seed_from_remote(&bytes);
                }
                if !synced_at_start {
                    return;
                }
                let summary = crate::replay::replay_missed_spendable_outputs(
                    &close_records,
                    &*chain_monitor,
                    &*chain_source,
                    &engine,
                    &logger,
                )
                .await;
                if summary.descriptors_tracked > 0 {
                    // Same wake the `SpendableOutputs` event arm uses: the
                    // existing engine, cadence, and `pending_sweep()` surface
                    // do the rest — no second queue, no new banner.
                    wake.notify_one();
                }
            });
        }
        self.spawn_recovery_task(
            &runtime,
            Arc::clone(&sweep_engine) as Arc<dyn RecoverySweeper>,
            stop_sender.subscribe(),
        );
        let bp_handle = spawn_background_processor(
            &runtime,
            &components,
            Arc::clone(&liquidity_source),
            Arc::clone(&self.payment_store),
            Arc::clone(&self.close_records),
            Arc::clone(&self.recovery),
            sweep_engine,
            sweep_wake,
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

    /// Base URL for block-explorer transaction links on this build's network.
    /// Available while stopped — it is configuration, not node state.
    pub fn explorer_base_url(&self) -> String {
        self.config.explorer_url.clone()
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

        // U10: detach the VSS halves so no close-record/recovery write is
        // scheduled onto the runtime being dropped. Local persistence (the
        // source of truth) keeps working while stopped.
        self.close_records.detach_vss();
        self.recovery.detach_vss();

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
        // payment store cached from the OLD wallet — and likewise the close
        // records and recovery banner (U10): the replaced wallet's state
        // must not survive (or be re-persisted over) the restored one.
        self.payment_store.reset();
        self.close_records.reset();
        self.recovery.reset();
        self.sweep_store.reset();
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

    /// The current force-close recovery state (U10, R9), or `None` when no
    /// recovery is active. Readable while stopped (local-first store).
    pub fn recovery_state(&self) -> Option<RecoveryState> {
        self.recovery.state()
    }

    /// Dismiss the recovery success banner (U14/U19, R9): durably clears the
    /// recovery state, but only once the sweep is confirmed — an active
    /// `NeedsRecovery` state is chain-truth-owned and never user-dismissible
    /// (PWA `use-recovery.ts` dismiss semantics).
    pub fn dismiss_recovery(&self) {
        if let Some(state) = self.recovery.state() {
            if matches!(
                state.status,
                crate::recovery::RecoveryStatus::SweepConfirmed
            ) {
                self.recovery.clear();
            }
        }
    }

    /// Outputs still waiting to sweep (U11, R8), `None` when nothing is
    /// pending. `pending_sats` is a LOWER BOUND (`has_unknown_value` marks
    /// undercounting); `needs_onchain_funds`/`shortfall_sats` drive the
    /// add-funds UX. Readable while stopped; changes arrive as
    /// `SweepStateChanged` events.
    pub fn pending_sweep(&self) -> Option<PendingSweepInfo> {
        self.sweep_store.pending_info()
    }

    /// One close record with the last-known tip height (U10) for the detail
    /// screen's per-tx roles and live confirmation counts. Readable while
    /// stopped.
    pub(crate) fn close_record_with_tip(
        &self,
        channel_id: &str,
    ) -> Option<(CloseRecord, Option<u32>)> {
        let record = self.close_records.get(channel_id)?;
        Some((record, self.close_records.last_tip_height()))
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    use lightning::types::payment::PaymentHash;

    use crate::history::{PaymentDirection, PaymentStatus};

    #[derive(Default)]
    pub(super) struct CapturingSink(pub(super) Mutex<Vec<CoreEvent>>);

    impl EventSink for CapturingSink {
        fn emit(&self, event: CoreEvent) {
            self.0.lock().unwrap().push(event);
        }
    }

    // The helpers below are `pub(super)` so the sibling modules' own test
    // submodules can reuse them via `crate::node::tests::*`. This is the only
    // visibility the split widens, and it is test-only.

    pub(super) fn store_in(dir: &Path) -> PaymentStore {
        PaymentStore::new(
            Arc::new(FilesystemStore::new(dir.join("store"))),
            Arc::new(Logger),
        )
    }

    pub(super) fn offline_config(dir: &Path) -> Config {
        let mut config = Config::new(dir.to_str().unwrap().to_string());
        config.esplora_url = "http://127.0.0.1:1".to_string();
        config.rgs_url = "http://127.0.0.1:1/snapshot".to_string();
        // Offline suites run local-only; the VSS-enabled paths are covered by
        // the U3 tests with an injected mock transport.
        config.vss_disabled = true;
        config
    }

    pub(super) fn payment_hash(byte: u8) -> PaymentHash {
        PaymentHash([byte; 32])
    }

    /// [`offline_config`]'s overrides applied to an already-built config, so a
    /// test can pick the network and still run without a network.
    pub(super) fn offline_config_for(mut config: Config) -> Config {
        config.esplora_url = "http://127.0.0.1:1".to_string();
        config.rgs_url = "http://127.0.0.1:1/snapshot".to_string();
        config.vss_disabled = true;
        config
    }

    /// A real `BlindedMessagePath` to stand in for one a static invoice
    /// server operator would hand over (U3).
    pub(super) fn static_invoice_server_path(
    ) -> lightning::blinded_path::message::BlindedMessagePath {
        use lightning::blinded_path::message::{BlindedMessagePath, MessageContext};
        use lightning::sign::{KeysManager, NodeSigner as _, Recipient};

        let keys = KeysManager::new(&[9u8; 32], 0, 0, false);
        BlindedMessagePath::one_hop(
            keys.get_node_id(Recipient::Node).unwrap(),
            keys.get_receive_auth_key(),
            MessageContext::Custom(b"static-invoice-server".to_vec()),
            &keys,
            &bitcoin::secp256k1::Secp256k1::new(),
        )
    }

    /// U4/R4: two nodes over the same base path, on different networks, share
    /// nothing. Asserted through the node's OWN readers — the KV store,
    /// mnemonic, and lock all resolve from `config.storage_dir` — because
    /// review caught the first cut scoping the path in the builder only,
    /// leaving those readers pointed at the mainnet directory.
    #[test]
    fn two_networks_over_one_base_path_share_no_state() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().to_str().unwrap().to_string();

        let mainnet = Config::for_network(crate::config::WalletNetwork::Mainnet, base.clone());
        let mutiny = Config::for_network(crate::config::WalletNetwork::Mutinynet, base.clone());

        assert_eq!(mainnet.storage_dir, base, "mainnet keeps the base path");
        assert_ne!(mainnet.storage_dir, mutiny.storage_dir);

        // Start each in turn; each must mint its OWN mnemonic beneath its own
        // directory. A shared path would have the second node read the first's
        // seed words.
        let mainnet_node = Node::new(offline_config_for(mainnet.clone()));
        mainnet_node.start().expect("mainnet offline start");
        let mainnet_words = mainnet_node.reveal_mnemonic();
        mainnet_node.stop().unwrap();

        let mutiny_node = Node::new(offline_config_for(mutiny.clone()));
        mutiny_node.start().expect("mutinynet offline start");
        let mutiny_words = mutiny_node.reveal_mnemonic();
        mutiny_node.stop().unwrap();

        assert!(mainnet_words.is_some() && mutiny_words.is_some());
        assert_ne!(
            mainnet_words, mutiny_words,
            "each network's node must own its own seed, not read the other's"
        );
        assert!(std::path::Path::new(&mutiny.storage_dir).exists());
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
                .record_pending(&young, PaymentDirection::Outbound, 2_000, now_ms())
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

    // ---------- U3/U4 remote-key-surface regression guard ----------

    /// PERMANENT REGRESSION GUARD (lifecycle half) for the bug reported as
    /// `Restore failed: backup inconsistent: N remote key(s) are not
    /// explained by the monitor manifest or the fixed key set`.
    ///
    /// [`crate::restore::reconcile_backup_keys`] is the STRICT predicate: no
    /// key `listKeyVersions` reports may be anything other than the obfuscated
    /// form of a [`crate::vss::store::FIXED_REMOTE_KEYS`] entry or of a
    /// manifest entry. Restore itself is no longer that strict — it fetches
    /// each unexplained key and adopts the ones that turn out to be this
    /// wallet's channel monitors — but that rescue only covers MONITORS. A
    /// non-monitor blob (a store handed the VSS-backed store instead of the
    /// plain local one, or a new remote blob never added to the shared fixed
    /// list) can never deserialize as a monitor, so it still bricks restore
    /// for that seed permanently, and the failure would only surface months
    /// later at the worst possible moment. Hence the strict predicate stays
    /// the write path's contract.
    ///
    /// So boot a REAL VSS-enabled node over a recording transport, drive the
    /// whole fresh-wallet lifecycle — start (fresh channel manager, the BDK
    /// persister, the network graph, the scorer, the LDK event queue and the
    /// liquidity manager's own event queue all persist), a known-peer write,
    /// a close record, a force-close recovery state, stop — and then run the
    /// PRODUCTION reconciliation against the resulting server listing. Also
    /// diff every key the transport was ever ASKED to store, which catches
    /// writes that were later deleted.
    ///
    /// If this test ever fails, the offending plaintext key is printed: fix
    /// the WIRING (route it at the plain local `FilesystemStore`), or, if the
    /// key genuinely belongs in the backup, add it to `FIXED_REMOTE_KEYS` —
    /// the one list both the writers and reconcile read. Never widen
    /// reconcile alone: it is the guard that catches a manifest undercounting
    /// monitors, which is a fund-safety check.
    #[test]
    fn fresh_wallet_lifecycle_never_writes_a_remote_key_restore_cannot_explain() {
        use crate::close_records::CloseRecord;
        use crate::config::VssTransportOverride;
        use crate::restore::reconcile_backup_keys;
        use crate::vss::store::{
            parse_monitor_manifest, VssTransport, FIXED_REMOTE_KEYS, MONITOR_MANIFEST_KEY,
        };
        use crate::vss::test_support::MockTransport;

        let dir = tempfile::tempdir().unwrap();
        let transport = Arc::new(MockTransport::new());
        let mut config = offline_config(dir.path());
        config.vss_disabled = false;
        config.vss_transport_override = Some(VssTransportOverride(
            Arc::clone(&transport) as Arc<dyn VssTransport>
        ));

        let node = Node::new(config);
        node.start().expect("fresh VSS-enabled offline start");

        // Give the startup tasks, the background processor and the liquidity
        // manager a chance to persist through their stores.
        std::thread::sleep(Duration::from_millis(750));

        // U9/R10: a saved peer — the `_known_peers` LWW blob.
        node.channel_handles()
            .expect("the node is running")
            .known_peers
            .upsert(
                "02eadbd9e7557375161df8b646776a547c5cbc2e95b3071ec81553f8ec2cea3b8c",
                "127.0.0.1",
                9735,
            )
            .expect("known-peer persist");

        // U10/R9: the close-records map and the force-close recovery blob.
        let channel_id = "cd".repeat(32);
        node.close_records
            .upsert(CloseRecord::skeleton(&channel_id, 1_700_000_000_000));
        node.recovery
            .enter(&channel_id, Some(25_000), || None, None, 1_700_000_000_000);

        std::thread::sleep(Duration::from_millis(750));
        node.stop().unwrap();

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let listing = rt.block_on(transport.list_key_versions()).unwrap();
        assert!(
            !listing.is_empty(),
            "a VSS-enabled fresh wallet must back the channel manager up remotely; an empty \
             listing would make this guard vacuous"
        );

        // The manifest as a restoring client would read it.
        let manifest_keys: Vec<String> = match rt.block_on(transport.get(MONITOR_MANIFEST_KEY)) {
            Ok(Some((bytes, _))) => {
                parse_monitor_manifest(&bytes).expect("our own manifest parses")
            }
            _ => Vec::new(),
        };

        // 1. The production predicate over the production listing.
        if let Err(e) = reconcile_backup_keys(&listing, &manifest_keys, &*transport) {
            let explained: std::collections::BTreeSet<String> = FIXED_REMOTE_KEYS
                .iter()
                .map(|k| k.to_string())
                .chain(manifest_keys.iter().cloned())
                .collect();
            let offenders: Vec<&str> = listing
                .iter()
                .map(|(key, _)| key.as_str())
                .filter(|key| !explained.contains(*key))
                .collect();
            panic!(
                "a fresh wallet's own lifecycle produced a remote key restore cannot explain: \
                 {offenders:?}\nroute these at the local FilesystemStore, or add them to \
                 FIXED_REMOTE_KEYS if they truly belong in the backup.\nreconcile said: {e}"
            );
        }

        // 2. Every key the transport was ever ASKED to store, including any
        //    since deleted (the mock's `obfuscate` is the identity, so these
        //    ARE the plaintext keys).
        let explained: std::collections::BTreeSet<String> = FIXED_REMOTE_KEYS
            .iter()
            .map(|k| k.to_string())
            .chain(manifest_keys.iter().cloned())
            .collect();
        let attempted = transport.attempted_put_keys();
        let unexplained: Vec<&String> = attempted.difference(&explained).collect();
        assert!(
            unexplained.is_empty(),
            "these plaintext keys were written to VSS but restore cannot explain them: \
             {unexplained:?} (attempted: {attempted:?}, explained: {explained:?})"
        );

        // The lifecycle really did exercise the remote blob writers — a guard
        // over an empty write set proves nothing.
        for key in [
            crate::vss::store::CHANNEL_MANAGER_VSS_KEY,
            crate::vss::store::KNOWN_PEERS_VSS_KEY,
            crate::vss::store::CLOSE_RECORDS_VSS_KEY,
            crate::vss::store::FORCE_CLOSE_RECOVERY_VSS_KEY,
        ] {
            assert!(
                attempted.contains(key),
                "the lifecycle must exercise the {key} writer for this guard to mean anything"
            );
        }
    }
}
