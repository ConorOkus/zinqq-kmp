//! The exported FFI surface (U3, expanded by U5/KTD-5): the wallet
//! operations (`start`, `stop`, `receive_jit`, `send`, `next_event` +
//! `event_handled`) and the U5 queries (`balances`, `node_id`,
//! `list_activity`, `payment_detail`) plus the two demo fns in `lib.rs` and
//! the AE1 `derive_debug_info` helper (U1). One object, all business logic in
//! Rust (R14): queries are cheap and non-blocking; history stays readable
//! while the node is stopped.

use std::path::PathBuf;
use std::sync::Arc;

use lightning_persister::fs_store::FilesystemStore;

use crate::builder::{BuildError, KV_STORE_SUBDIR};
use crate::config::Config;
use crate::events::{Event, EventQueue};
use crate::history::{ActivityRow, PersistedPayment};
use crate::keys::{self, KeysError};
use crate::liquidity::Lsps2Error;
use crate::node::Node;
use crate::payment::SendError;
use crate::types::Logger;

/// FFI-facing configuration. Network is fixed to mainnet; the mnemonic is
/// auto-created in `storage_dir` on first start (U1, R1 — restore-from-words
/// arrives with U4); the overrides exist for tests and fallback and default
/// to the PWA's infrastructure (U12/KTD-12). Every new field has a uniffi
/// default, so existing shell call sites keep compiling.
#[derive(Debug, Clone, uniffi::Record)]
pub struct WalletConfig {
    /// App-private data directory holding the mnemonic and all persisted
    /// state.
    pub storage_dir: String,
    /// Esplora REST endpoint override (defaults to KTD-5's Zinqq proxy).
    pub esplora_url: Option<String>,
    /// Rapid Gossip Sync snapshot server override (defaults to KTD-6's LDK
    /// public server).
    pub rgs_url: Option<String>,
    /// VSS endpoint override (defaults to the Zinqq pass-through proxy,
    /// `https://zinqq.app/api/vss-proxy`).
    #[uniffi(default = None)]
    pub vss_url: Option<String>,
    /// Disables VSS entirely (local-only persistence) when `true`.
    #[uniffi(default = false)]
    pub vss_disabled: bool,
    /// Block-explorer base URL override (defaults to
    /// `https://mempool.space`).
    #[uniffi(default = None)]
    pub explorer_url: Option<String>,
    /// LSP override: node id (66-char hex pubkey). All three of
    /// `lsp_node_id`/`lsp_host`/`lsp_port` must be set together (defaults to
    /// Megalith from the PWA's config).
    #[uniffi(default = None)]
    pub lsp_node_id: Option<String>,
    /// LSP override: host (IP or DNS name).
    #[uniffi(default = None)]
    pub lsp_host: Option<String>,
    /// LSP override: port.
    #[uniffi(default = None)]
    pub lsp_port: Option<u16>,
    /// Extra LSP node ids trusted for 0-conf inbound channels, on top of the
    /// Megalith seed and the configured LSP (KTD-10: a set + predicate).
    #[uniffi(default = [])]
    pub trusted_lsp_node_ids: Vec<String>,
}

/// Wallet balances, from U2's bdk wallet and channel monitors, split per U5:
/// spendable is bdk's trusted spendable (confirmed + trusted pending);
/// total additionally includes untrusted pending (unconfirmed external
/// receives).
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct Balances {
    /// Sum of all claimable lightning channel balances, in msat.
    pub lightning_msat: u64,
    /// Total on-chain balance (confirmed + all pending), in sats.
    pub onchain_total_sats: u64,
    /// Trusted-spendable on-chain balance (confirmed + trusted pending), in
    /// sats.
    pub onchain_spendable_sats: u64,
    /// Unconfirmed sats received from external wallets (untrusted pending).
    pub onchain_untrusted_pending_sats: u64,
}

/// Typed FFI errors (Kotlin `WalletException`).
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Error)]
pub enum WalletError {
    /// A [`WalletConfig`] override failed to parse (bad LSP node id/address,
    /// or an incomplete LSP override triple).
    InvalidConfig { detail: String },
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
    /// A mnemonic that is not a valid BIP39 English 12-word mnemonic (U1).
    InvalidMnemonic,
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
            WalletError::InvalidConfig { detail } => {
                write!(f, "invalid wallet configuration: {detail}")
            }
            WalletError::AlreadyRunning => write!(f, "the node is already running"),
            WalletError::InstanceAlreadyRunning => write!(
                f,
                "another wallet instance is already running against this storage directory"
            ),
            WalletError::NotRunning => write!(f, "the node is not running"),
            WalletError::Startup { detail } => write!(f, "failed to start the node: {detail}"),
            WalletError::InvalidMnemonic => write!(
                f,
                "the mnemonic is not a valid BIP39 English 12-word mnemonic"
            ),
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

impl From<KeysError> for WalletError {
    fn from(error: KeysError) -> Self {
        match error {
            KeysError::InvalidMnemonic => WalletError::InvalidMnemonic,
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

/// Applies the FFI config's overrides over the core defaults (U12). Pure and
/// separate from the constructor so the override/validation matrix is
/// unit-testable.
fn apply_config_overrides(config: WalletConfig) -> Result<Config, WalletError> {
    use std::str::FromStr as _;

    let mut core_config = Config::new(config.storage_dir);
    if let Some(esplora_url) = config.esplora_url {
        core_config.esplora_url = esplora_url;
    }
    if let Some(rgs_url) = config.rgs_url {
        core_config.rgs_url = rgs_url;
    }
    if let Some(vss_url) = config.vss_url {
        core_config.vss_url = vss_url;
    }
    core_config.vss_disabled = config.vss_disabled;
    if let Some(explorer_url) = config.explorer_url {
        core_config.explorer_url = explorer_url;
    }

    match (config.lsp_node_id, config.lsp_host, config.lsp_port) {
        (None, None, None) => {}
        (Some(node_id), Some(host), Some(port)) => {
            core_config.lsp.node_id =
                bitcoin::secp256k1::PublicKey::from_str(&node_id).map_err(|e| {
                    WalletError::InvalidConfig {
                        detail: format!("lsp_node_id is not a valid public key: {e}"),
                    }
                })?;
            core_config.lsp.address =
                format!("{host}:{port}")
                    .parse()
                    .map_err(|e| WalletError::InvalidConfig {
                        detail: format!("lsp_host/lsp_port is not a valid ip:port address: {e}"),
                    })?;
        }
        _ => {
            return Err(WalletError::InvalidConfig {
                detail: "lsp_node_id, lsp_host, and lsp_port must be set together".to_string(),
            })
        }
    }

    for trusted in config.trusted_lsp_node_ids {
        let node_id = bitcoin::secp256k1::PublicKey::from_str(&trusted).map_err(|e| {
            WalletError::InvalidConfig {
                detail: format!("trusted LSP node id {trusted} is not a valid public key: {e}"),
            }
        })?;
        if !core_config.trusted_lsps.contains(&node_id) {
            core_config.trusted_lsps.push(node_id);
        }
    }

    Ok(core_config)
}

/// AE1 debug helper (U1, R2): the node id a 12-word mnemonic yields — the
/// same value the PWA reports for the same words, so cross-client identity
/// can be verified without moving funds.
#[uniffi::export]
pub fn derive_debug_info(mnemonic: String) -> Result<String, WalletError> {
    keys::derive_debug_info(&mnemonic).map_err(WalletError::from)
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
    /// previously persisted (unacked) events from the storage dir. Fails with
    /// [`WalletError::InvalidConfig`] when an override fails to parse (U12) —
    /// a misconfigured override must never silently fall back to defaults.
    #[uniffi::constructor]
    pub fn new(config: WalletConfig) -> Result<Self, WalletError> {
        let core_config = apply_config_overrides(config)?;

        let kv_store = Arc::new(FilesystemStore::new(
            PathBuf::from(&core_config.storage_dir).join(KV_STORE_SUBDIR),
        ));
        let events = Arc::new(EventQueue::new(kv_store, Arc::new(Logger)));
        let node = Node::with_event_sink(core_config, Arc::clone(&events) as _);
        Ok(Self { node, events })
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
        let onchain = self
            .node
            .onchain_balances()
            .ok_or(WalletError::NotRunning)?;
        Ok(Balances {
            lightning_msat,
            onchain_total_sats: onchain.total_sats,
            onchain_spendable_sats: onchain.spendable_sats,
            onchain_untrusted_pending_sats: onchain.untrusted_pending_sats,
        })
    }

    /// This node's public key (66-char hex); requires a running node.
    pub fn node_id(&self) -> Result<String, WalletError> {
        self.node
            .node_id()
            .map(|node_id| node_id.to_string())
            .ok_or(WalletError::NotRunning)
    }

    /// The unified activity feed (U5, KTD-7), merged and sorted in core
    /// (R14: shells never merge): Lightning rows with failed hidden,
    /// on-chain transactions as net amounts with close-absorbed txids
    /// skipped, one row per close record, descending by time. Requires a
    /// running node (the on-chain arm reads the bdk wallet).
    pub fn list_activity(&self) -> Result<Vec<ActivityRow>, WalletError> {
        self.node.list_activity().ok_or(WalletError::NotRunning)
    }

    /// One persisted payment row by payment id (U5) — e.g. an activity row's
    /// `payment_hash`. Includes FAILED payments the feed hides. Readable
    /// while the node is stopped.
    pub fn payment_detail(&self, payment_id: String) -> Option<PersistedPayment> {
        self.node.payment_detail(&payment_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr as _;

    use crate::config::{
        DEFAULT_ESPLORA_URL, DEFAULT_EXPLORER_URL, DEFAULT_RGS_URL, DEFAULT_VSS_URL,
        MEGALITH_LSP_NODE_ID,
    };

    fn base_config() -> WalletConfig {
        WalletConfig {
            storage_dir: "/tmp/data".to_string(),
            esplora_url: None,
            rgs_url: None,
            vss_url: None,
            vss_disabled: false,
            explorer_url: None,
            lsp_node_id: None,
            lsp_host: None,
            lsp_port: None,
            trusted_lsp_node_ids: Vec::new(),
        }
    }

    /// U12/KTD-12: no overrides yields the PWA's infrastructure defaults.
    #[test]
    fn no_overrides_yield_the_pwa_infrastructure_defaults() {
        let config = apply_config_overrides(base_config()).unwrap();
        assert_eq!(config.esplora_url, DEFAULT_ESPLORA_URL);
        assert_eq!(config.rgs_url, DEFAULT_RGS_URL);
        assert_eq!(config.vss_url, DEFAULT_VSS_URL);
        assert!(!config.vss_disabled);
        assert_eq!(config.explorer_url, DEFAULT_EXPLORER_URL);
        assert_eq!(
            config.lsp.node_id,
            bitcoin::secp256k1::PublicKey::from_str(MEGALITH_LSP_NODE_ID).unwrap()
        );
    }

    #[test]
    fn overrides_apply_and_the_lsp_override_is_trusted() {
        let other = "02eec7245d6b7d2ccb30380bfbe2a3648cd7a942653f5aa340edcea1f283686619";
        let extra = "03864ef025fde8fb587d989186ce6a4a186895ee44a926bfc370e2c366597a3f8f";
        let mut ffi_config = base_config();
        ffi_config.vss_url = Some("http://127.0.0.1:1/vss".to_string());
        ffi_config.vss_disabled = true;
        ffi_config.explorer_url = Some("http://127.0.0.1:1/explorer".to_string());
        ffi_config.lsp_node_id = Some(other.to_string());
        ffi_config.lsp_host = Some("127.0.0.1".to_string());
        ffi_config.lsp_port = Some(9736);
        ffi_config.trusted_lsp_node_ids = vec![extra.to_string()];

        let config = apply_config_overrides(ffi_config).unwrap();
        assert_eq!(config.vss_url, "http://127.0.0.1:1/vss");
        assert!(config.vss_disabled);
        assert_eq!(config.explorer_url, "http://127.0.0.1:1/explorer");
        assert_eq!(config.lsp.address.to_string(), "127.0.0.1:9736");
        let other = bitcoin::secp256k1::PublicKey::from_str(other).unwrap();
        let extra = bitcoin::secp256k1::PublicKey::from_str(extra).unwrap();
        assert_eq!(config.lsp.node_id, other);
        assert!(config.is_trusted_lsp(&other), "the LSP override is trusted");
        assert!(config.is_trusted_lsp(&extra), "extra trusted ids are added");
        assert!(
            config.is_trusted_lsp(
                &bitcoin::secp256k1::PublicKey::from_str(MEGALITH_LSP_NODE_ID).unwrap()
            ),
            "the Megalith seed survives an LSP override"
        );
    }

    /// U12: a misconfigured override is a typed error with a distinct
    /// message, never a silent fallback to defaults.
    #[test]
    fn invalid_overrides_fail_with_typed_config_errors() {
        let mut partial = base_config();
        partial.lsp_node_id = Some(MEGALITH_LSP_NODE_ID.to_string());
        let err = apply_config_overrides(partial).unwrap_err();
        assert!(matches!(err, WalletError::InvalidConfig { .. }), "{err}");
        assert!(err.to_string().contains("must be set together"), "{err}");

        let mut bad_key = base_config();
        bad_key.trusted_lsp_node_ids = vec!["not-a-key".to_string()];
        let err = apply_config_overrides(bad_key).unwrap_err();
        assert!(matches!(err, WalletError::InvalidConfig { .. }), "{err}");
        assert!(err.to_string().contains("not-a-key"), "{err}");

        let mut bad_addr = base_config();
        bad_addr.lsp_node_id = Some(MEGALITH_LSP_NODE_ID.to_string());
        bad_addr.lsp_host = Some("not an address".to_string());
        bad_addr.lsp_port = Some(9735);
        let err = apply_config_overrides(bad_addr).unwrap_err();
        assert!(matches!(err, WalletError::InvalidConfig { .. }), "{err}");
    }
}
