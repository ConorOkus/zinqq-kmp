//! Node lifecycle (KTD-3, KTD-10): the `Node` owns a 2-worker tokio runtime
//! created at `start()` and dropped at `stop()`. The background processor runs
//! via `process_events_async_with_kv_store_sync` with
//! `mobile_interruptable_platform = true`; periodic chain sync, fee refresh,
//! RGS refresh, broadcast draining, and peer reconnects run as runtime tasks
//! stopped through watch channels.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::str::FromStr as _;
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
use lightning::sign::EntropySource as _;
use lightning::types::payment::PaymentHash;
use lightning::util::logger::Logger as _;
use lightning::util::ser::Writeable as _;
use lightning_background_processor::{process_events_async_with_kv_store_sync, GossipSync};
use lightning_persister::fs_store::FilesystemStore;
use tokio::runtime::Runtime;
use tokio::sync::watch;

use crate::builder::{build, persist_channel_manager, BuildError, NodeComponents, KV_STORE_SUBDIR};
use crate::channels::{
    self, ChannelEventContext, ChannelView, ChannelsError, CloseEstimate, FundingStore,
    OpenFeeEstimate, PeerView,
};
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
    describe_failure_reason, parse_and_validate, payment_id_for, resolve_amount, send_bolt11,
    send_bolt12, validate_offer, SendError,
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

/// The handles the U9 peer/channel calls need, cloned out of the state lock
/// so no dial or list ever holds it.
struct ChannelHandles {
    channel_manager: Arc<crate::types::ChannelManager>,
    chain_monitor: Arc<crate::types::ChainMonitor>,
    peer_manager: Arc<crate::types::PeerManager>,
    known_peers: Arc<crate::vss::known_peers::KnownPeersStore>,
    onchain_wallet: Arc<crate::wallet::OnchainWallet>,
    chain_source: Arc<crate::chain::ChainSource>,
    liquidity_source: Arc<LiquiditySource>,
    runtime_handle: tokio::runtime::Handle,
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
        let (liquidity_source, runtime_handle) = self.liquidity_handles()?;

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

    /// Clones the LSPS2 handles out of the state lock (the receive_jit
    /// pattern: spawn on the node runtime, wait outside the lock).
    fn liquidity_handles(
        &self,
    ) -> Result<(Arc<LiquiditySource>, tokio::runtime::Handle), Lsps2Error> {
        let state_lock = self.state.lock().unwrap();
        let state = state_lock.as_ref().ok_or(Lsps2Error::NotRunning)?;
        Ok((
            Arc::clone(&state.liquidity_source),
            state.runtime.handle().clone(),
        ))
    }

    /// U7 phase A (F2 quote step): `get_info` + cheapest-valid selection
    /// against the configured LSP — NO `buy`, no LSP-side commitment. The
    /// quote carries fee/net/validity/freshness for the review screen and a
    /// single-use token for [`Node::jit_accept`]. AE4: below-floor amounts
    /// fail here with a typed error, so no buy can ever follow them.
    /// Blocking (LSP round-trip): call from a background dispatcher.
    ///
    /// Quote failures return typed errors WITHOUT queueing `Lsps2Failed`:
    /// below-minimum is a review-screen state in the PWA, not an incident.
    pub fn jit_quote(&self, amount_msat: u64) -> Result<crate::receive::JitQuote, Lsps2Error> {
        let (liquidity_source, runtime_handle) = self.liquidity_handles()?;
        let (result_sender, result_receiver) = tokio::sync::oneshot::channel();
        runtime_handle.spawn(async move {
            let _ = result_sender.send(liquidity_source.jit_quote(amount_msat).await);
        });
        result_receiver
            .blocking_recv()
            .unwrap_or(Err(Lsps2Error::Shutdown))
    }

    /// U7 phase B (F2 buy step): consumes the quote token, clamps the
    /// invoice expiry to the quote's remaining validity (R6: `valid_until`
    /// − 30 s, capped at 3600 s, under 60 s → the typed
    /// [`Lsps2Error::QuoteExpired`] re-quote signal BEFORE any `buy`), then
    /// buys and mints the wrapped invoice. On success the invoice is ALSO
    /// pushed as `InvoiceReady` with the clamped expiry; failures are pushed
    /// as `Lsps2Failed`. Blocking: call from a background dispatcher.
    pub fn jit_accept(
        &self,
        quote_token: u64,
        amount_msat: u64,
    ) -> Result<crate::receive::JitInvoice, Lsps2Error> {
        let (liquidity_source, runtime_handle) = self.liquidity_handles()?;
        let (result_sender, result_receiver) = tokio::sync::oneshot::channel();
        runtime_handle.spawn(async move {
            let _ = result_sender.send(liquidity_source.jit_accept(quote_token, amount_msat).await);
        });
        let result = result_receiver
            .blocking_recv()
            .unwrap_or(Err(Lsps2Error::Shutdown));

        match result {
            Ok((invoice, expires_at_unix, opening_fee_msat)) => {
                let bolt11 = invoice.to_string();
                self.event_sink.emit(CoreEvent::InvoiceReady {
                    bolt11: bolt11.clone(),
                    expiry_unix_secs: expires_at_unix,
                });
                Ok(crate::receive::JitInvoice {
                    bolt11,
                    payment_hash: hex_str(&invoice.payment_hash().to_byte_array()),
                    opening_fee_msat,
                    expires_at_unix,
                })
            }
            Err(error) => {
                self.event_sink.emit(CoreEvent::Lsps2Failed {
                    reason: error.to_string(),
                });
                Err(error)
            }
        }
    }

    /// The JIT numpad floor in sats (U7, R6, AE4): one amountless `get_info`
    /// per receive session (`refresh = true` starts a new session), cached
    /// and NEVER an error — failures and empty menus degrade to the static
    /// 3,000-sat floor, as does a stopped node. Mirrors the PWA's
    /// only-when-it-can-matter gate (`Receive.tsx:158-161`): when usable
    /// inbound capacity already covers the static floor the fetch is skipped
    /// entirely — any below-floor amount is served by existing capacity.
    /// Blocking on a fetch: call from a background dispatcher.
    pub fn min_receive_sats(&self, refresh: bool) -> u64 {
        let (liquidity_source, runtime_handle, channel_manager) = {
            let state_lock = self.state.lock().unwrap();
            match state_lock.as_ref() {
                None => return crate::receive::MIN_JIT_RECEIVE_SATS,
                Some(state) => (
                    Arc::clone(&state.liquidity_source),
                    state.runtime.handle().clone(),
                    Arc::clone(&state.components.channel_manager),
                ),
            }
        };
        let usable_inbound_msat: u64 = channel_manager
            .list_channels()
            .iter()
            .filter(|details| details.is_usable)
            .map(|details| details.inbound_capacity_msat)
            .sum();
        if usable_inbound_msat >= crate::receive::MIN_JIT_RECEIVE_SATS * 1_000 {
            return crate::receive::MIN_JIT_RECEIVE_SATS;
        }
        let (result_sender, result_receiver) = tokio::sync::oneshot::channel();
        runtime_handle.spawn(async move {
            let _ = result_sender.send(liquidity_source.min_receive_sats(refresh).await);
        });
        result_receiver
            .blocking_recv()
            .unwrap_or(crate::receive::MIN_JIT_RECEIVE_SATS)
    }

    /// A standard (non-JIT) BOLT11 invoice via the channel manager's
    /// `create_inbound_payment`-based builder (U7): description
    /// `Zinqq Wallet`, 3600 s expiry, amountless allowed — the PWA's
    /// `createInvoice` verbatim. Returns `(bolt11, payment_hash_hex)`; paid
    /// detection rides the payment store (U5).
    pub fn standard_invoice(
        &self,
        amount_msat: Option<u64>,
    ) -> Result<(String, String), crate::receive::ReceiveError> {
        let channel_manager = {
            let state_lock = self.state.lock().unwrap();
            let state = state_lock
                .as_ref()
                .ok_or(crate::receive::ReceiveError::NotRunning)?;
            Arc::clone(&state.components.channel_manager)
        };
        let invoice = channel_manager
            .create_bolt11_invoice(crate::receive::standard_invoice_params(amount_msat))
            .map_err(|_| crate::receive::ReceiveError::InvoiceCreationFailed)?;
        Ok((
            invoice.to_string(),
            hex_str(&invoice.payment_hash().to_byte_array()),
        ))
    }

    /// The one receive call the shells render (U7, R6): on-chain address,
    /// standard invoice when capacity covers the request (`needs_jit` false),
    /// the unified BIP321 URI in copy and QR forms, the persisted offer when
    /// a usable channel exists, the session floor, and the capacity decision.
    /// Never touches the network: the floor is the session-cached value (use
    /// [`Node::min_receive_sats`] to fetch), and only the ALREADY-persisted
    /// offer is included (use [`Node::get_or_create_offer`] to mint one) —
    /// offer creation never blocks receive.
    pub fn receive_bundle(
        &self,
        amount_msat: Option<u64>,
    ) -> Result<crate::receive::ReceiveBundle, crate::receive::ReceiveError> {
        use crate::receive::{self, ReceiveError};

        let (kv_store, liquidity_source) = {
            let state_lock = self.state.lock().unwrap();
            let state = state_lock.as_ref().ok_or(ReceiveError::NotRunning)?;
            (
                Arc::clone(&state.components.kv_store),
                Arc::clone(&state.liquidity_source),
            )
        };
        let address =
            self.next_receive_address()
                .map_err(|e| ReceiveError::AddressUnavailable {
                    detail: e.to_string(),
                })?;
        let channels = self.list_channels().map_err(|_| ReceiveError::NotRunning)?;

        let needs_jit = receive::needs_jit(&channels, amount_msat);
        let (bolt11, payment_hash, invoice_error) = if needs_jit {
            // JIT path (amounted) or amountless-with-no-capacity: the PWA
            // renders the on-chain QR and drives the quote flow separately.
            (None, None, None)
        } else {
            match self.standard_invoice(amount_msat) {
                Ok((bolt11, payment_hash)) => (Some(bolt11), Some(payment_hash), None),
                // Receive.tsx:289-291: the failure copy renders only for an
                // amounted request; the on-chain QR still shows either way.
                Err(error) => (
                    None,
                    None,
                    amount_msat
                        .filter(|amount| *amount > 0)
                        .map(|_| error.to_string()),
                ),
            }
        };

        let amount_sats = amount_msat.map(|msat| msat / 1_000);
        let bip321_uri = receive::build_bip321_uri(&address, amount_sats, bolt11.as_deref());
        // QR alphanumeric mode uppercases the WHOLE URI (Receive.tsx:640).
        let qr_value = bip321_uri.to_uppercase();

        // showBolt12 gating (Receive.tsx:372): an offer page exists only
        // when an offer is persisted AND a usable channel can pay it.
        let offer = if receive::has_usable_channel(&channels) {
            receive::read_persisted_offer(&kv_store)
        } else {
            None
        };
        let offer_qr_value = offer
            .as_deref()
            .map(|offer| receive::build_bolt12_page_uri(offer).to_uppercase());

        Ok(receive::ReceiveBundle {
            address,
            bolt11,
            payment_hash,
            invoice_error,
            bip321_uri,
            qr_value,
            offer,
            offer_qr_value,
            needs_jit,
            min_receive_sats: liquidity_source
                .cached_jit_floor_sats()
                .unwrap_or(receive::MIN_JIT_RECEIVE_SATS),
        })
    }

    /// The persistent BOLT12 offer (U7, R6): returns the persisted one when
    /// it exists; otherwise creates it via `create_offer_builder` (chain
    /// mainnet, description `zinqq wallet` — the PWA's builder calls,
    /// `context.tsx:1655-1658`), retrying on the 3/6/12/24/48 s schedule
    /// because blinded paths need the RGS-synced graph. Persisted under a
    /// stable local key on success. `None` on a stopped node or when every
    /// attempt failed — offer creation NEVER blocks receive. Blocking (up to
    /// the retry schedule): call from a background dispatcher.
    pub fn get_or_create_offer(&self) -> Option<String> {
        use crate::config::BOLT12_OFFER_DESCRIPTION;

        let (channel_manager, kv_store, runtime_handle, logger) = {
            let state_lock = self.state.lock().unwrap();
            let state = state_lock.as_ref()?;
            (
                Arc::clone(&state.components.channel_manager),
                Arc::clone(&state.components.kv_store),
                state.runtime.handle().clone(),
                Arc::clone(&state.components.logger),
            )
        };
        if let Some(existing) = crate::receive::read_persisted_offer(&kv_store) {
            return Some(existing);
        }

        let network = self.config.network;
        let attempt_logger = Arc::clone(&logger);
        let (result_sender, result_receiver) = tokio::sync::oneshot::channel();
        runtime_handle.spawn(async move {
            let result = crate::receive::create_offer_with_retry(
                || {
                    let build = || -> Result<String, String> {
                        let offer = channel_manager
                            .create_offer_builder()
                            .map_err(|e| format!("create_offer_builder: {e:?}"))?
                            .chain(network)
                            .description(BOLT12_OFFER_DESCRIPTION.to_string())
                            .build()
                            .map_err(|e| format!("offer build: {e:?}"))?;
                        Ok(offer.to_string())
                    };
                    build().inspect_err(|reason| {
                        log_error!(
                            attempt_logger,
                            "BOLT12 offer creation attempt failed (graph not ready?): {reason}"
                        );
                    })
                },
                &crate::receive::OFFER_RETRY_DELAYS,
            )
            .await;
            let _ = result_sender.send(result);
        });
        let offer = result_receiver.blocking_recv().ok().flatten()?;

        // PWA parity (context.tsx:1663): the offer is exposed only once
        // persisted, so every later session serves the SAME offer string.
        match crate::receive::persist_offer(&kv_store, &offer) {
            Ok(()) => Some(offer),
            Err(e) => {
                log_error!(logger, "Failed to persist the BOLT12 offer: {e}");
                None
            }
        }
    }

    /// Whether the BOLT12 offer pager page should exist (U7, R6): a
    /// persisted offer AND at least one usable channel (the PWA's
    /// `showBolt12`, `Receive.tsx:372`). `false` while stopped.
    pub fn offer_available(&self) -> bool {
        let (channel_manager, kv_store) = {
            let state_lock = self.state.lock().unwrap();
            match state_lock.as_ref() {
                None => return false,
                Some(state) => (
                    Arc::clone(&state.components.channel_manager),
                    Arc::clone(&state.components.kv_store),
                ),
            }
        };
        channel_manager
            .list_channels()
            .iter()
            .any(|details| details.is_usable)
            && crate::receive::read_persisted_offer(&kv_store).is_some()
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
    pub fn send_payment(
        &self,
        bolt11: &str,
        amount_override_msat: Option<u64>,
    ) -> Result<(), SendError> {
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
        // U6: the amount override (for amountless invoices) resolves here,
        // so the row records the amount actually being sent.
        let (invoice, amount_msat) =
            parse_and_validate(bolt11, self.config.network, now, amount_override_msat)?;
        let payment_id_hex = hex_str(&payment_id_for(&invoice).0);
        let payment_hash_hex = hex_str(invoice.payment_hash().as_byte_array());
        if let Err(e) = self.payment_store.record_pending(
            &payment_id_hex,
            PaymentDirection::Outbound,
            amount_msat,
            now.as_millis() as u64,
        ) {
            log_error!(
                Logger,
                "Failed to write the pending history row for {payment_id_hex}: {e}"
            );
        }

        let result = send_bolt11(
            &*channel_manager,
            bolt11,
            self.config.network,
            now,
            amount_override_msat,
        );
        match result {
            Ok(_payment_id) => Ok(()),
            Err(error) => {
                self.settle_attempt_failure(&payment_id_hex, Some(payment_hash_hex), &error);
                Err(error)
            }
        }
    }

    /// Pays a mainnet BOLT12 offer (U6, R5). Blocking (LSP dial + offer
    /// machinery): call from a background dispatcher.
    ///
    /// PWA `sendBolt12Payment` parity (`context.tsx:1026-1091`): the LSP is
    /// connected first so invoice-request onion messages can route, the
    /// payment id is 32 random bytes (BOLT12 payments have no payment hash
    /// until the invoice arrives), `payer_note` rides the invoice request,
    /// and retries are ×3. The pending history row is keyed by that random
    /// payment id; `PaymentSent`/`PaymentFailed` settle it by the same id
    /// (U5's row-key rule prefers `payment_id` when present).
    pub fn pay_offer(
        &self,
        offer_str: &str,
        amount_override_msat: Option<u64>,
        payer_note: Option<String>,
    ) -> Result<(), SendError> {
        let (channel_manager, keys_manager, liquidity_source, runtime_handle) = {
            let state_lock = self.state.lock().unwrap();
            let state = state_lock.as_ref().ok_or(SendError::NotRunning)?;
            (
                Arc::clone(&state.components.channel_manager),
                Arc::clone(&state.components.keys_manager),
                Arc::clone(&state.liquidity_source),
                state.runtime.handle().clone(),
            )
        };
        let now = unix_now();

        // Validation failures return before anything is attempted or
        // recorded (same contract as send_payment).
        let (_offer, embedded_msat) = validate_offer(offer_str, self.config.network, now)?;
        let amount_msat = resolve_amount(embedded_msat, amount_override_msat)?;

        let payment_id = PaymentId(keys_manager.get_secure_random_bytes());
        let payment_id_hex = hex_str(&payment_id.0);
        if let Err(e) = self.payment_store.record_pending(
            &payment_id_hex,
            PaymentDirection::Outbound,
            amount_msat,
            now.as_millis() as u64,
        ) {
            log_error!(
                Logger,
                "Failed to write the pending history row for {payment_id_hex}: {e}"
            );
        }

        // LSP pre-connect for onion transport (PWA context.tsx:1032-1044):
        // without a connected LSP the invoice request cannot route. Run on
        // the node runtime, wait outside the state lock (receive_jit's
        // pattern). A connect failure fails the payment, like the PWA's
        // thrown connectAndTrack.
        let (result_sender, result_receiver) = tokio::sync::oneshot::channel();
        runtime_handle.spawn(async move {
            let _ = result_sender.send(liquidity_source.ensure_lsp_connected().await);
        });
        let connected = result_receiver
            .blocking_recv()
            .unwrap_or(Err(Lsps2Error::Shutdown));

        let result = match connected {
            Err(error) => Err(SendError::SendFailed(format!(
                "could not connect to the LSP for BOLT12 onion messaging: {error}"
            ))),
            Ok(()) => send_bolt12(
                &*channel_manager,
                offer_str,
                self.config.network,
                now,
                amount_override_msat,
                payer_note,
                payment_id,
            )
            .map(|_amount| ()),
        };
        match result {
            Ok(()) => Ok(()),
            Err(error) => {
                // No payment hash yet: BOLT12 failures before an invoice
                // arrives carry None (events.rs PaymentFailed contract).
                self.settle_attempt_failure(&payment_id_hex, None, &error);
                Err(error)
            }
        }
    }

    /// Shared U5/U6 handling for synchronous attempt failures: LDK abandoned
    /// without queueing an event, so settle the row and push the public
    /// failure ourselves, row first (the row must never lag the event it
    /// explains). Validation failures and duplicates skip this — nothing was
    /// attempted (or the original attempt owns the outcome).
    fn settle_attempt_failure(
        &self,
        payment_id_hex: &str,
        payment_hash_hex: Option<String>,
        error: &SendError,
    ) {
        if !error.is_attempt_failure() {
            return;
        }
        if let Err(e) = self.payment_store.settle(
            payment_id_hex,
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
            payment_hash: payment_hash_hex,
            reason: error.to_string(),
        });
    }

    /// Test-only: one real `lsps2.get_info` round-trip (the plan's live
    /// Megalith smoke test).
    #[cfg(test)]
    pub(crate) fn lsps2_get_info_live(
        &self,
    ) -> Result<Vec<lightning_liquidity::lsps2::msgs::LSPS2OpeningFeeParams>, Lsps2Error> {
        let (liquidity_source, runtime_handle) = self.liquidity_handles()?;
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

    /// Clones the U9 peer/channel handles out of the state lock, so no call
    /// below ever blocks while holding it.
    fn channel_handles(&self) -> Result<ChannelHandles, ChannelsError> {
        let state_lock = self.state.lock().unwrap();
        let state = state_lock.as_ref().ok_or(ChannelsError::NotRunning)?;
        Ok(ChannelHandles {
            channel_manager: Arc::clone(&state.components.channel_manager),
            chain_monitor: Arc::clone(&state.components.chain_monitor),
            peer_manager: Arc::clone(&state.components.peer_manager),
            known_peers: Arc::clone(&state.components.known_peers),
            onchain_wallet: Arc::clone(&state.components.onchain_wallet),
            chain_source: Arc::clone(&state.components.chain_source),
            liquidity_source: Arc::clone(&state.liquidity_source),
            runtime_handle: state.runtime.handle().clone(),
        })
    }

    /// Dials `node_id` at `socket_addr` (waiting for the BOLT8 handshake) and
    /// persists it to the known-peers store on success — the PWA's
    /// `connectToPeer` semantics (`context.tsx:746-755`). The configured LSP
    /// is dialed through the liquidity source's connect lock instead, so a
    /// racing `receive_jit` is never stranded on a dropped duplicate socket.
    fn dial_and_persist(
        &self,
        handles: &ChannelHandles,
        node_id: PublicKey,
        socket_addr: std::net::SocketAddr,
    ) -> Result<(), ChannelsError> {
        // Run on the node runtime, wait outside the state lock (the
        // receive_jit pattern): a dropped runtime surfaces as a closed
        // channel, not a hang.
        let (result_sender, result_receiver) = tokio::sync::oneshot::channel();
        if node_id == self.config.lsp.node_id {
            let liquidity_source = Arc::clone(&handles.liquidity_source);
            handles.runtime_handle.spawn(async move {
                let result = liquidity_source.ensure_lsp_connected().await.map_err(|e| {
                    ChannelsError::ConnectFailed {
                        detail: e.to_string(),
                    }
                });
                let _ = result_sender.send(result);
            });
        } else {
            let peer_manager = Arc::clone(&handles.peer_manager);
            handles.runtime_handle.spawn(async move {
                let _ = result_sender
                    .send(channels::dial_peer(peer_manager, node_id, socket_addr).await);
            });
        }
        result_receiver
            .blocking_recv()
            .unwrap_or(Err(ChannelsError::ConnectFailed {
                detail: "the node is shutting down".to_string(),
            }))?;

        // Persist AFTER a successful connect, best-effort surfaced as typed.
        handles
            .known_peers
            .upsert(
                &node_id.to_string(),
                &socket_addr.ip().to_string(),
                socket_addr.port(),
            )
            .map_err(|e| ChannelsError::PersistFailed {
                detail: e.to_string(),
            })
    }

    /// Connects to a `pubkey@host:port` peer and saves it as a known peer
    /// (U9, R10). Blocking (dial + handshake): call from a background
    /// dispatcher. Returns the peer's pubkey hex.
    pub fn connect_peer(&self, address: &str) -> Result<String, ChannelsError> {
        let (node_id, socket_addr) = channels::parse_peer_address(address)?;
        let handles = self.channel_handles()?;
        self.dial_and_persist(&handles, node_id, socket_addr)?;
        Ok(node_id.to_string())
    }

    /// Disconnects a peer's socket (U9). Does NOT forget it: the reconnect
    /// loop will keep dialing saved peers (PWA `disconnectPeer`).
    pub fn disconnect_peer(&self, pubkey: &str) -> Result<(), ChannelsError> {
        let node_id = PublicKey::from_str(pubkey).map_err(|_| ChannelsError::InvalidPubkey)?;
        let handles = self.channel_handles()?;
        handles.peer_manager.disconnect_by_node_id(node_id);
        Ok(())
    }

    /// Removes a saved peer (U9, R10). Refused with
    /// [`ChannelsError::PeerHasOpenChannels`] while any channel with the
    /// peer is open (PWA `forgetPeer`, `context.tsx:852-868`).
    pub fn forget_peer(&self, pubkey: &str) -> Result<(), ChannelsError> {
        let handles = self.channel_handles()?;
        channels::ensure_no_open_channels_with(
            handles
                .channel_manager
                .list_channels()
                .iter()
                .map(|details| details.counterparty.node_id.to_string()),
            pubkey,
        )?;
        handles
            .known_peers
            .remove(pubkey)
            .map_err(|e| ChannelsError::PersistFailed {
                detail: e.to_string(),
            })
    }

    /// The Peers screen's rows (U9, R10): the union of saved and connected
    /// peers, connected first (PWA `Peers.tsx:79-99`).
    pub fn list_peers(&self) -> Result<Vec<PeerView>, ChannelsError> {
        let handles = self.channel_handles()?;
        let connected: HashSet<String> = handles
            .peer_manager
            .list_peers()
            .iter()
            .map(|details| details.counterparty_node_id.to_string())
            .collect();
        let mut channel_counts: HashMap<String, u32> = HashMap::new();
        for details in handles.channel_manager.list_channels() {
            *channel_counts
                .entry(details.counterparty.node_id.to_string())
                .or_insert(0) += 1;
        }
        Ok(channels::build_peer_views(
            &handles.known_peers.all(),
            &connected,
            &channel_counts,
        ))
    }

    /// Every channel as a Peers-screen row (U9, R10), including the in-flight
    /// HTLC count the close screen's warning uses.
    pub fn list_channels(&self) -> Result<Vec<ChannelView>, ChannelsError> {
        let handles = self.channel_handles()?;
        Ok(handles
            .channel_manager
            .list_channels()
            .iter()
            .map(channels::channel_view)
            .collect())
    }

    /// The open-channel review numbers (U9): the 6-block rate × 140 vB (PWA
    /// `OpenChannel.tsx:68-72,97-98`).
    pub fn estimate_open_fee(&self) -> Result<OpenFeeEstimate, ChannelsError> {
        let handles = self.channel_handles()?;
        Ok(channels::open_fee_estimate(
            handles.chain_source.onchain_send_fee_rate_sat_per_vb(),
        ))
    }

    /// Opens a channel to `pubkey@host:port` (U9, R10): bounds
    /// 20,000–16,777,215 sats, balance gate at amount + estimated fee,
    /// connect-if-needed (persisting the known peer), then `create_channel`
    /// with an 8-byte random `user_channel_id` (PWA `OpenChannel.tsx` +
    /// `context.tsx:757-780`). Blocking: call from a background dispatcher.
    /// Returns the TEMPORARY channel id hex; the funding flow proceeds via
    /// the event switchboard (FundingGenerationReady → persist-then-notify →
    /// FundingTxBroadcastSafe → broadcast).
    pub fn open_channel(&self, address: &str, amount_sats: u64) -> Result<String, ChannelsError> {
        channels::check_open_amount(amount_sats)?;
        let (node_id, socket_addr) = channels::parse_peer_address(address)?;
        let handles = self.channel_handles()?;

        // The PWA's balance gate (`OpenChannel.tsx:97-101`): amount plus the
        // 6-block × 140 vB estimate must fit the spendable balance.
        let estimate =
            channels::open_fee_estimate(handles.chain_source.onchain_send_fee_rate_sat_per_vb());
        if amount_sats + estimate.estimated_fee_sats
            > handles.onchain_wallet.trusted_spendable_sats()
        {
            return Err(ChannelsError::AmountExceedsBalance);
        }

        self.dial_and_persist(&handles, node_id, socket_addr)?;

        let temporary_channel_id = handles
            .channel_manager
            .create_channel(
                node_id,
                amount_sats,
                0,
                channels::random_user_channel_id(),
                None,
                None,
            )
            .map_err(|e| ChannelsError::OpenFailed {
                detail: format!("{e:?}"),
            })?;
        Ok(hex_str(&temporary_channel_id.0))
    }

    /// Closes a channel (U9, R10): cooperative `close_channel` or
    /// `force_close_broadcasting_latest_txn` with the PWA's reason string
    /// (`context.tsx:783-813`).
    pub fn close_channel(&self, channel_id_hex: &str, force: bool) -> Result<(), ChannelsError> {
        let handles = self.channel_handles()?;
        let details = handles
            .channel_manager
            .list_channels()
            .into_iter()
            .find(|details| hex_str(&details.channel_id.0) == channel_id_hex)
            .ok_or(ChannelsError::ChannelNotFound)?;
        let result = if force {
            handles.channel_manager.force_close_broadcasting_latest_txn(
                &details.channel_id,
                &details.counterparty.node_id,
                channels::FORCE_CLOSE_REASON.to_string(),
            )
        } else {
            handles
                .channel_manager
                .close_channel(&details.channel_id, &details.counterparty.node_id)
        };
        result.map_err(|e| ChannelsError::CloseFailed {
            detail: format!("{e:?}"),
        })
    }

    /// The informational pre-close estimate (U9, R10): nullable per field
    /// and NEVER an error — a stopped node or unknown channel returns the
    /// all-`None` estimate, so the close screen always renders (PWA
    /// `estimate.ts` contract).
    pub fn estimate_close(&self, channel_id_hex: &str) -> CloseEstimate {
        let Ok(handles) = self.channel_handles() else {
            return CloseEstimate::unavailable();
        };
        channels::estimate_close(
            &handles.channel_manager,
            &handles.chain_monitor,
            &handles.chain_source.fee_estimator(),
            channel_id_hex,
        )
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
/// THEN the public event — persist-then-ack), the U9 channel-open funding
/// flow (FundingGenerationReady → persist-then-notify,
/// FundingTxBroadcastSafe → broadcast, DiscardFunding → cleanup,
/// ChannelClosed → auto-forget), and log-and-ack for the rest.
fn handle_ldk_event(
    event: Event,
    sweeper: &Sweeper,
    liquidity_source: &LiquiditySource,
    payment_store: &PaymentStore,
    channels_ctx: &ChannelEventContext,
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
        Event::ChannelPending {
            channel_id,
            former_temporary_channel_id,
            ..
        } => {
            let channel_id_hex = hex_str(&channel_id.0);
            // U9: record real→temporary so DiscardFunding (which carries the
            // real id) can clean up the funding tx keyed by the temp id (the
            // PWA's `ldk_channel_id_map`, `event-handler.ts:352-356`).
            if let Some(temp_id) = former_temporary_channel_id {
                channels_ctx
                    .funding
                    .record_channel_id_map(&channel_id_hex, &hex_str(&temp_id.0));
            }
            event_sink.emit(CoreEvent::ChannelPending {
                channel_id: channel_id_hex,
            });
            Ok(())
        }
        Event::ChannelReady { channel_id, .. } => {
            event_sink.emit(CoreEvent::ChannelReady {
                channel_id: hex_str(&channel_id.0),
            });
            Ok(())
        }
        // U9 funding flow: build from the on-chain wallet, persist the tx
        // BEFORE notifying LDK, and broadcast only on FundingTxBroadcastSafe.
        Event::FundingGenerationReady {
            temporary_channel_id,
            counterparty_node_id,
            channel_value_satoshis,
            output_script,
            ..
        } => {
            channels::on_funding_generation_ready(
                channels_ctx,
                temporary_channel_id,
                counterparty_node_id,
                channel_value_satoshis,
                output_script,
                logger,
            );
            Ok(())
        }
        Event::FundingTxBroadcastSafe {
            former_temporary_channel_id,
            ..
        } => {
            let temp_hex = hex_str(&former_temporary_channel_id.0);
            match channels::handle_funding_tx_broadcast_safe(
                &channels_ctx.funding,
                &channels_ctx.broadcaster,
                &temp_hex,
            ) {
                channels::BroadcastSafeOutcome::Broadcast { txid } => {
                    log_info!(logger, "Broadcast funding tx {txid} for {temp_hex}");
                }
                channels::BroadcastSafeOutcome::MissingTx => {
                    log_error!(
                        logger,
                        "FundingTxBroadcastSafe: no persisted funding tx for {temp_hex}"
                    );
                }
            }
            Ok(())
        }
        Event::DiscardFunding { channel_id, .. } => {
            let channel_id_hex = hex_str(&channel_id.0);
            if channels::handle_discard_funding(&channels_ctx.funding, &channel_id_hex) {
                log_info!(
                    logger,
                    "DiscardFunding: dropped the persisted funding tx for {channel_id_hex}"
                );
            }
            Ok(())
        }
        Event::ChannelClosed {
            channel_id,
            reason,
            counterparty_node_id,
            ..
        } => {
            let channel_id_hex = hex_str(&channel_id.0);
            // U9 auto-forget: the LAST channel with a peer closing drops it
            // from known peers, so the reconnect loop stops dialing it (PWA
            // `context.tsx:1233-1244`). Best-effort, never a replay.
            if let Some(counterparty) = counterparty_node_id {
                let still_has_channels = channels_ctx
                    .channel_manager
                    .list_channels()
                    .iter()
                    .any(|details| details.counterparty.node_id == counterparty);
                channels::auto_forget_on_channel_closed(
                    &channels_ctx.known_peers,
                    &counterparty.to_string(),
                    still_has_channels,
                    logger,
                );
            }
            // Funding-map cleanup (PWA `event-handler.ts:446`).
            channels_ctx.funding.remove_channel_id_map(&channel_id_hex);
            event_sink.emit(CoreEvent::ChannelClosed {
                channel_id: channel_id_hex,
                reason: reason.to_string(),
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
    // U9: the channel-event context (funding flow + auto-forget handles).
    // The funding store rides on the raw local KVStore — funding txs are
    // local-only, like the PWA's IDB stores, never on VSS.
    let channels_ctx = Arc::new(ChannelEventContext {
        channel_manager: Arc::clone(&components.channel_manager),
        onchain_wallet: Arc::clone(&components.onchain_wallet),
        broadcaster: Arc::clone(&components.broadcaster),
        chain_source: Arc::clone(&components.chain_source),
        known_peers: Arc::clone(&components.known_peers),
        funding: Arc::new(FundingStore::new(
            Arc::clone(&components.kv_store),
            Arc::clone(&components.logger),
        )),
    });
    let event_handler = move |event: Event| {
        let sweeper = Arc::clone(&event_sweeper);
        let liquidity_source = Arc::clone(&liquidity_source);
        let payment_store = Arc::clone(&payment_store);
        let channels_ctx = Arc::clone(&channels_ctx);
        let event_sink = Arc::clone(&event_sink);
        let logger = Arc::clone(&event_logger);
        async move {
            handle_ldk_event(
                event,
                &sweeper,
                &liquidity_source,
                &payment_store,
                &channels_ctx,
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

    /// U7 at the Node seam: receive endpoints follow the lifecycle. An
    /// offline degraded start (fresh wallet, zero channels) serves the
    /// on-chain-only bundle; the standard invoice carries the PWA's
    /// description/expiry and allows amountless; a persisted offer stays
    /// gated on usable channels (and survives a restart under its stable
    /// key); a bogus accept token fails typed and queues `Lsps2Failed`.
    #[test]
    fn receive_endpoints_follow_the_node_lifecycle() {
        use crate::receive::{ReceiveError, MIN_JIT_RECEIVE_SATS};

        let dir = tempfile::tempdir().unwrap();
        let sink = Arc::new(CapturingSink::default());
        let node = Node::with_event_sink(offline_config(dir.path()), Arc::clone(&sink) as _);

        // Stopped: typed NotRunning; the floor and offer degrade safely.
        assert_eq!(
            node.jit_quote(1_000_000).unwrap_err(),
            Lsps2Error::NotRunning
        );
        assert_eq!(
            node.jit_accept(1, 1_000_000).unwrap_err(),
            Lsps2Error::NotRunning
        );
        assert_eq!(
            node.receive_bundle(None).unwrap_err(),
            ReceiveError::NotRunning
        );
        assert_eq!(
            node.standard_invoice(None).unwrap_err(),
            ReceiveError::NotRunning
        );
        assert_eq!(node.min_receive_sats(false), MIN_JIT_RECEIVE_SATS);
        assert_eq!(node.get_or_create_offer(), None);
        assert!(!node.offer_available());

        node.start().expect("offline degraded start");

        // Fresh wallet, amountless: the on-chain-only QR state
        // (Receive.tsx:209-218) — needs_jit, no invoice, no error copy.
        let bundle = node.receive_bundle(None).unwrap();
        assert!(bundle.needs_jit, "no usable channel");
        assert_eq!(bundle.bolt11, None);
        assert_eq!(bundle.payment_hash, None);
        assert_eq!(bundle.invoice_error, None);
        assert!(bundle.address.starts_with("bc1q"), "BIP84 mainnet address");
        assert_eq!(
            bundle.bip321_uri,
            format!("bitcoin:{}", bundle.address.to_uppercase())
        );
        assert_eq!(bundle.qr_value, bundle.bip321_uri.to_uppercase());
        assert_eq!(bundle.offer, None);
        assert_eq!(bundle.offer_qr_value, None);
        assert_eq!(bundle.min_receive_sats, MIN_JIT_RECEIVE_SATS);

        // Amounted while JIT is needed: the amount rides the URI (the QR
        // stays scannable on-chain) and no lightning param exists yet.
        let bundle = node.receive_bundle(Some(5_000_000)).unwrap();
        assert!(bundle.needs_jit);
        assert_eq!(bundle.bolt11, None);
        assert!(
            bundle.bip321_uri.ends_with("?amount=0.00005000"),
            "unexpected URI: {}",
            bundle.bip321_uri
        );

        // The standard invoice mirrors the PWA's createInvoice: amountless
        // allowed, description 'Zinqq Wallet', 3600 s expiry, and the
        // returned hash matches the invoice's.
        let (bolt11, payment_hash_hex) = node.standard_invoice(None).unwrap();
        let invoice = lightning_invoice::Bolt11Invoice::from_str(&bolt11).unwrap();
        assert_eq!(invoice.amount_milli_satoshis(), None, "amountless allowed");
        assert_eq!(invoice.expiry_time(), Duration::from_secs(3_600));
        assert_eq!(invoice.description().to_string(), "Zinqq Wallet");
        assert_eq!(
            payment_hash_hex,
            hex_str(&invoice.payment_hash().to_byte_array())
        );
        let (amounted, _) = node.standard_invoice(Some(250_000)).unwrap();
        assert_eq!(
            lightning_invoice::Bolt11Invoice::from_str(&amounted)
                .unwrap()
                .amount_milli_satoshis(),
            Some(250_000)
        );

        // A persisted offer does NOT surface with zero usable channels
        // (showBolt12 gating), but get_or_create_offer serves it verbatim
        // instead of minting a new one.
        let kv_store = FilesystemStore::new(dir.path().join(KV_STORE_SUBDIR));
        crate::receive::persist_offer(&kv_store, "lno1testoffer").unwrap();
        assert!(!node.offer_available(), "zero usable channels → no page");
        assert_eq!(node.receive_bundle(None).unwrap().offer, None);
        assert_eq!(node.get_or_create_offer().as_deref(), Some("lno1testoffer"));

        // A bogus accept token: typed error, Lsps2Failed queued, no buy.
        assert_eq!(
            node.jit_accept(999, 1_000_000).unwrap_err(),
            Lsps2Error::QuoteNotFound
        );
        assert!(
            sink.0.lock().unwrap().iter().any(|event| matches!(
                event,
                CoreEvent::Lsps2Failed { reason } if reason.contains("no longer available")
            )),
            "the failure must reach the event queue"
        );

        node.stop().unwrap();

        // The offer is restart-stable under its stable key.
        node.start().expect("offline degraded restart");
        assert_eq!(node.get_or_create_offer().as_deref(), Some("lno1testoffer"));
        node.stop().unwrap();
    }

    /// U9 at the Node seam: every peer/channel endpoint is NotRunning while
    /// stopped (except estimate_close, which NEVER errors); once started
    /// (offline, degraded) the lists are empty, the open-fee estimate answers
    /// from the offline default rate, bounds and the balance gate fire before
    /// any dial, an unreachable peer fails typed, and closes of unknown
    /// channels are ChannelNotFound.
    #[test]
    fn channel_endpoints_follow_the_node_lifecycle() {
        let dir = tempfile::tempdir().unwrap();
        let node = Node::new(offline_config(dir.path()));
        // Deliberately NOT the configured LSP's pubkey: the LSP is dialed
        // through the liquidity connect lock at its CONFIGURED address, so an
        // LSP-pubkey test would leave the offline sandbox.
        const PEER: &str =
            "02eec7245d6b7d2ccb30380bfbe2a3648cd7a942653f5aa340edcea1f283686619@127.0.0.1:1";
        const PEER_PUBKEY: &str =
            "02eec7245d6b7d2ccb30380bfbe2a3648cd7a942653f5aa340edcea1f283686619";

        // Stopped: typed NotRunning everywhere; estimate_close still answers.
        assert_eq!(
            node.connect_peer(PEER).unwrap_err(),
            ChannelsError::NotRunning
        );
        assert_eq!(node.list_peers().unwrap_err(), ChannelsError::NotRunning);
        assert_eq!(node.list_channels().unwrap_err(), ChannelsError::NotRunning);
        assert_eq!(
            node.open_channel(PEER, 50_000).unwrap_err(),
            ChannelsError::NotRunning
        );
        assert_eq!(
            node.estimate_close(&"11".repeat(32)),
            CloseEstimate::unavailable(),
            "estimate_close never errors, even stopped"
        );

        // Validation fires before the running check reaches a dial: bounds
        // and address parsing are typed regardless.
        assert_eq!(
            node.open_channel(PEER, 19_999).unwrap_err(),
            ChannelsError::AmountBelowMinimum
        );
        assert_eq!(
            node.open_channel(PEER, 16_777_216).unwrap_err(),
            ChannelsError::AmountAboveMaximum
        );
        assert!(matches!(
            node.connect_peer("junk").unwrap_err(),
            ChannelsError::InvalidAddress(_)
        ));

        node.start().expect("offline degraded start");

        // Fresh wallet: no peers, no channels; the open-fee estimate answers
        // from the PWA's offline 6-block default (5 sat/vB × 140 vB).
        assert_eq!(node.list_peers().unwrap(), Vec::new());
        assert_eq!(node.list_channels().unwrap(), Vec::new());
        assert_eq!(
            node.estimate_open_fee().unwrap(),
            crate::channels::OpenFeeEstimate {
                fee_rate_sat_per_vb: 5,
                estimated_fee_sats: 700,
            }
        );
        // The balance gate fires BEFORE any dial (empty wallet).
        assert_eq!(
            node.open_channel(PEER, 50_000).unwrap_err(),
            ChannelsError::AmountExceedsBalance
        );
        // An unreachable peer fails typed, and nothing was persisted.
        assert!(matches!(
            node.connect_peer(PEER).unwrap_err(),
            ChannelsError::ConnectFailed { .. }
        ));
        assert_eq!(node.list_peers().unwrap(), Vec::new());
        // Forgetting with zero channels is allowed (idempotent no-op here).
        assert_eq!(node.forget_peer(PEER_PUBKEY), Ok(()));
        assert_eq!(
            node.disconnect_peer("junk").unwrap_err(),
            ChannelsError::InvalidPubkey
        );
        assert_eq!(node.disconnect_peer(PEER_PUBKEY), Ok(()));
        // Unknown channel: typed not-found for closes, all-None estimate.
        assert_eq!(
            node.close_channel(&"22".repeat(32), false).unwrap_err(),
            ChannelsError::ChannelNotFound
        );
        assert_eq!(
            node.close_channel(&"22".repeat(32), true).unwrap_err(),
            ChannelsError::ChannelNotFound
        );
        assert_eq!(
            node.estimate_close(&"22".repeat(32)),
            CloseEstimate::unavailable()
        );
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
            node.send_payment(&invoice.to_string(), None),
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
            node.send_payment("junk", None),
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
