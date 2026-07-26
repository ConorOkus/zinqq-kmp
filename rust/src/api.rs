//! The exported FFI surface (U3). Deliberately tiny — exactly the six wallet
//! operations (`start`, `stop`, `receive_jit`, `send`, `next_event` +
//! `event_handled`, `balances`) plus U1's two demo fns in `lib.rs`; Gobley
//! risk shrinks with API size.

use std::path::PathBuf;
use std::sync::Arc;

use lightning_persister::fs_store::FilesystemStore;

use crate::builder::{BuildError, KV_STORE_SUBDIR};
use crate::config::Config;
use crate::events::{Event, EventQueue};
use crate::liquidity::Lsps2Error;
use crate::node::Node;
use crate::payment::SendError;
use crate::types::Logger;

/// FFI-facing configuration. Network is fixed to mainnet and there is no
/// seed/mnemonic input (AE2); the URL overrides exist for tests and fallback.
#[derive(Debug, Clone, uniffi::Record)]
pub struct WalletConfig {
    /// App-private data directory holding the seed and all persisted state.
    pub storage_dir: String,
    /// Esplora REST endpoint override (defaults to KTD-5's Zinqq proxy).
    pub esplora_url: Option<String>,
    /// Rapid Gossip Sync snapshot server override (defaults to KTD-6's LDK
    /// public server).
    pub rgs_url: Option<String>,
}

/// Wallet balances, from U2's bdk wallet and channel monitors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct Balances {
    /// Sum of all claimable lightning channel balances, in msat.
    pub lightning_msat: u64,
    /// Total on-chain balance (confirmed + pending), in sats.
    pub onchain_sats: u64,
}

/// Typed FFI errors (Kotlin `WalletException`).
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Error)]
pub enum WalletError {
    /// `start()` while already running.
    AlreadyRunning,
    /// Another node already holds this storage directory's lock — a second
    /// wallet instance (another activity, another process) tried to start over
    /// the same seed. Refused: two live nodes diverge on channel state.
    InstanceAlreadyRunning,
    /// An operation that needs a running node while stopped.
    NotRunning,
    /// The node failed to start (restore/persistence/config problem).
    Startup { detail: String },
    /// `event_handled()` with no event pending — an ack without a handle.
    NoPendingEvent,
    /// The LSPS2 JIT flow failed; `reason` is the same distinct reason the
    /// corresponding [`Event::Lsps2Failed`] carries.
    Lsps2 { reason: String },
    /// `send()` with a bolt11 string that failed to parse or verify.
    InvalidInvoice { detail: String },
    /// `send()` with an invoice that is already expired.
    InvoiceExpired,
    /// `send()` with an invoice for a different network (this wallet pays
    /// mainnet invoices only); `network` names the invoice's network.
    WrongNetwork { network: String },
    /// `send()` with an amountless invoice — the spike sends fixed-amount
    /// invoices only, and there is no amount argument to supply one.
    AmountlessInvoice,
    /// `send()` of an invoice whose payment is already pending — paying again
    /// would risk paying twice, so the original attempt owns the outcome.
    DuplicatePayment,
    /// The send attempt failed (e.g. no route); `reason` is the same distinct
    /// reason the corresponding [`Event::PaymentFailed`] carries.
    SendFailed { reason: String },
}

impl std::fmt::Display for WalletError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WalletError::AlreadyRunning => write!(f, "the node is already running"),
            WalletError::InstanceAlreadyRunning => write!(
                f,
                "another wallet instance is already running against this storage directory"
            ),
            WalletError::NotRunning => write!(f, "the node is not running"),
            WalletError::Startup { detail } => write!(f, "failed to start the node: {detail}"),
            WalletError::NoPendingEvent => write!(f, "no event is pending an ack"),
            WalletError::Lsps2 { reason } => write!(f, "LSPS2 request failed: {reason}"),
            WalletError::InvalidInvoice { detail } => {
                write!(f, "invalid bolt11 invoice: {detail}")
            }
            WalletError::InvoiceExpired => write!(f, "the invoice is expired"),
            WalletError::WrongNetwork { network } => write!(
                f,
                "the invoice is for the {network} network, this wallet only pays bitcoin \
                 (mainnet) invoices"
            ),
            WalletError::AmountlessInvoice => write!(
                f,
                "the invoice has no amount; amountless invoices are not supported"
            ),
            WalletError::DuplicatePayment => {
                write!(f, "a payment for this invoice is already pending")
            }
            WalletError::SendFailed { reason } => write!(f, "sending failed: {reason}"),
        }
    }
}

impl std::error::Error for WalletError {}

impl From<BuildError> for WalletError {
    fn from(error: BuildError) -> Self {
        match error {
            BuildError::AlreadyRunning => WalletError::AlreadyRunning,
            BuildError::InstanceAlreadyRunning => WalletError::InstanceAlreadyRunning,
            BuildError::NotRunning => WalletError::NotRunning,
            other => WalletError::Startup {
                detail: other.to_string(),
            },
        }
    }
}

impl From<SendError> for WalletError {
    fn from(error: SendError) -> Self {
        match error {
            SendError::NotRunning => WalletError::NotRunning,
            SendError::InvalidInvoice(message) => WalletError::InvalidInvoice { detail: message },
            SendError::InvoiceExpired => WalletError::InvoiceExpired,
            SendError::WrongNetwork { found, .. } => WalletError::WrongNetwork {
                network: found.to_string(),
            },
            SendError::AmountMissing => WalletError::AmountlessInvoice,
            SendError::DuplicatePayment => WalletError::DuplicatePayment,
            // Attempt failures: the same reason string the queued
            // Event::PaymentFailed carries.
            other @ (SendError::RouteNotFound | SendError::SendFailed(_)) => {
                WalletError::SendFailed {
                    reason: other.to_string(),
                }
            }
        }
    }
}

/// The one FFI object: a node handle plus the persisted event queue.
///
/// The queue owns its own `FilesystemStore` handle over the same store
/// directory the node uses, created at wallet construction — so events can be
/// pushed, persisted, and CONSUMED while the node (and its runtime) is
/// stopped. That independence is what makes the KTD-8 lifecycle contract
/// hold: `stop()` pushes a terminal [`Event::NodeStopped`] whose wake-up
/// travels through the queue's `Notify`, not through any runtime IO, so a
/// pending `next_event` — polled by the foreign executor via UniFFI —
/// completes promptly even though the node's runtime is gone.
#[derive(uniffi::Object)]
pub struct Wallet {
    node: Node,
    events: Arc<EventQueue>,
}

#[uniffi::export]
impl Wallet {
    /// Creates a stopped wallet over the given config, reloading any
    /// previously persisted (unacked) events from the storage dir.
    #[uniffi::constructor]
    pub fn new(config: WalletConfig) -> Self {
        let mut core_config = Config::new(config.storage_dir);
        if let Some(esplora_url) = config.esplora_url {
            core_config.esplora_url = esplora_url;
        }
        if let Some(rgs_url) = config.rgs_url {
            core_config.rgs_url = rgs_url;
        }

        let kv_store = Arc::new(FilesystemStore::new(
            PathBuf::from(&core_config.storage_dir).join(KV_STORE_SUBDIR),
        ));
        let events = Arc::new(EventQueue::new(kv_store, Arc::new(Logger)));
        let node = Node::with_event_sink(core_config, Arc::clone(&events) as _);
        Self { node, events }
    }

    /// Starts the node. Blocking (initial restore + sync attempt): call from
    /// a background dispatcher.
    ///
    /// `NodeStarted` is queued as soon as the node is up, WITHOUT waiting for
    /// chain sync to reach the tip — a degraded offline start emits
    /// `NodeStarted` then `SyncFailed`, so the queue is observable with no
    /// network (KTD-8).
    pub fn start(&self) -> Result<(), WalletError> {
        self.node.start()?;
        // Purge any stale NodeStopped a previous process persisted but never
        // acked: it is terminal for the shells' event loops, so redelivering
        // it now would exit the loop while the node runs. It only ever had
        // meaning for completing a pending next_event in the process that
        // pushed it.
        self.events
            .retain(|event| !matches!(event, Event::NodeStopped));
        self.events.push(Event::NodeStarted);
        if !self.node.is_chain_synced() {
            self.events.push(Event::SyncFailed);
        }
        Ok(())
    }

    /// Stops the node and pushes the terminal `NodeStopped` event, completing
    /// any pending `next_event` await (KTD-8 lifecycle contract).
    pub fn stop(&self) -> Result<(), WalletError> {
        let result = self.node.stop();
        // NodeStopped is pushed whenever the node actually transitioned to
        // stopped — including a stop whose final persistence write failed —
        // so a pending next_event never hangs. Only a no-op stop() (not
        // running) skips it.
        if !matches!(result, Err(BuildError::NotRunning)) {
            self.events.push(Event::NodeStopped);
        }
        result.map_err(WalletError::from)
    }

    /// Requests a Megalith JIT invoice for `amount_msat`; the invoice arrives
    /// as [`Event::InvoiceReady`] (with its `valid_until` expiry), failures as
    /// [`Event::Lsps2Failed`] AND a typed error here. Blocking (LSP network
    /// round-trips): call from a background dispatcher.
    pub fn receive_jit(&self, amount_msat: u64) -> Result<(), WalletError> {
        self.node
            .receive_jit(amount_msat)
            .map(|_bolt11_and_expiry| ())
            .map_err(|error| match error {
                Lsps2Error::NotRunning => WalletError::NotRunning,
                other => WalletError::Lsps2 {
                    reason: other.to_string(),
                },
            })
    }

    /// Pays a mainnet BOLT11 invoice; the outcome arrives as
    /// [`Event::PaymentSuccessful`] / [`Event::PaymentFailed`]. Blocking
    /// (route computation): call from a background dispatcher.
    ///
    /// Idempotent across restarts (U5): the payment id is derived from the
    /// invoice's payment hash, so re-sending an in-flight invoice fails with
    /// [`WalletError::DuplicatePayment`] instead of paying twice. Invalid
    /// invoices (malformed / expired / wrong network / amountless) each fail
    /// with a distinct typed error before anything is attempted.
    pub fn send(&self, bolt11: String) -> Result<(), WalletError> {
        self.node.send_payment(&bolt11).map_err(WalletError::from)
    }

    /// Awaits the front event WITHOUT removing it (Kotlin `suspend`). The
    /// same event is returned until `event_handled` acks it. This future is
    /// polled by the foreign executor and never touches the node's runtime,
    /// so it stays valid across `stop()`.
    pub async fn next_event(&self) -> Event {
        self.events.next().await
    }

    /// Acks (pops) the front event — the second half of handle-then-ack.
    pub fn event_handled(&self) -> Result<(), WalletError> {
        self.events
            .ack()
            .map(|_| ())
            .ok_or(WalletError::NoPendingEvent)
    }

    /// Current balances; requires a running node.
    pub fn balances(&self) -> Result<Balances, WalletError> {
        let lightning_msat = self
            .node
            .lightning_balance_msat()
            .ok_or(WalletError::NotRunning)?;
        let onchain_sats = self
            .node
            .onchain_balance_sats()
            .ok_or(WalletError::NotRunning)?;
        Ok(Balances {
            lightning_msat,
            onchain_sats,
        })
    }
}
