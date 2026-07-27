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
use crate::channels::{ChannelView, ChannelsError, CloseEstimate, OpenFeeEstimate, PeerView};
use crate::close_records::{
    derive_close_status, CloseRecord, CloseTxRole, CloseType, Initiator, Resolution,
};
use crate::config::Config;
use crate::events::{Event, EventQueue};
use crate::history::{ActivityRow, CloseStatusLabel, PersistedPayment};
use crate::keys::{self, KeysError};
use crate::liquidity::Lsps2Error;
use crate::node::Node;
use crate::onchain_send::{FeeEstimate, MaxSendEstimate, OnchainSendError};
use crate::payment::SendError;
use crate::receive::{JitInvoice, JitQuote, ReceiveBundle, ReceiveError};
use crate::restore::RestoreError;
use crate::send::{self, Classified, HttpNameResolver, LnurlPayMetadata, ResolveError};
use crate::types::Logger;
use crate::util::unix_now;

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

/// A close record tx's role for the detail screen (U10). The names are the
/// screen labels; they map 1:1 onto the PWA's `CloseTxRole` wire strings
/// (`closing`/`commitment`/`anchor_cpfp`/`htlc_claim`/`sweep`); `Other`
/// carries roles from newer schema versions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum CloseTxRoleView {
    Closing,
    Commitment,
    FeeBump,
    PaymentClaim,
    SweepToWallet,
    Other,
}

/// One transaction attached to a close (U10 detail screen): role, fee, and
/// the live confirmation count against the last-known tip.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct CloseTxView {
    pub txid: String,
    pub role: CloseTxRoleView,
    pub fee_sats: Option<u64>,
    pub confirmed_at_height: Option<u32>,
    /// Confirmations vs the last-known tip; `None` while unconfirmed or the
    /// tip is unknown.
    pub confirmations: Option<u32>,
}

/// `'coop' | 'force' | 'unknown'` (U10).
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum CloseTypeView {
    Coop,
    Force,
    Unknown,
}

/// `'local' | 'remote' | 'unknown'` (U10).
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum CloseInitiatorView {
    Local,
    Remote,
    Unknown,
}

/// One close record for the detail screen (U10, R9): facts + the derived
/// status label + per-tx roles with live confirmation counts.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct CloseRecordView {
    pub channel_id: String,
    pub close_type: CloseTypeView,
    pub initiator: CloseInitiatorView,
    /// Human-readable closure description (PWA copy, verbatim).
    pub closure_reason: Option<String>,
    /// Derived per the PWA's `deriveCloseStatus` — never stored.
    pub status: CloseStatusLabel,
    /// LDK's last-known local balance at close — an estimate. `None` while
    /// unknown: render "—", never a lying 0.
    pub expected_amount_sats: Option<u64>,
    pub timelock_blocks: Option<u32>,
    /// Height at which timelocked funds become claimable.
    pub claimable_at_height: Option<u32>,
    /// Last-known tip height (for countdowns), `None` before the first
    /// reconcile pass.
    pub current_height: Option<u32>,
    pub created_at_ms: u64,
    pub completed_at_ms: Option<u64>,
    /// `true` when the record resolved WITHOUT wallet receipt evidence
    /// (rendered distinctly — never laundered into "complete").
    pub resolved_unverified: bool,
    pub funding_txid: Option<String>,
    pub funding_vout: Option<u32>,
    pub txs: Vec<CloseTxView>,
}

/// Recovery banner status (U10, R9).
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum RecoveryStatusView {
    NeedsRecovery,
    SweepConfirmed,
}

/// Outputs still waiting to sweep (U11, R8): the pending banner + add-funds
/// UX. `pending_sats` is a LOWER BOUND — `has_unknown_value` marks
/// undercounting; `needs_onchain_funds`/`shortfall_sats` mean a subsidized
/// sweep would rescue the funds once the confirmed balance grows by the
/// shortfall. Changes arrive as [`Event::SweepStateChanged`]; re-read on
/// every one (the PWA's `usePendingSweep`).
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct PendingSweepView {
    pub entry_count: u32,
    pub descriptor_count: u32,
    pub pending_sats: u64,
    pub has_unknown_value: bool,
    pub last_attempt_failed: bool,
    pub needs_onchain_funds: bool,
    pub shortfall_sats: Option<u64>,
}

/// The force-close recovery state for the banner/screen (U10, R9).
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct RecoveryStateView {
    pub status: RecoveryStatusView,
    /// Estimated stuck balance; `None` = unknown (render "Unknown").
    pub stuck_balance_sat: Option<u64>,
    pub deposit_address: String,
    pub deposit_needed_sat: u64,
    pub channel_ids: Vec<String>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

fn close_record_view(record: &CloseRecord, tip: Option<u32>) -> CloseRecordView {
    CloseRecordView {
        channel_id: record.channel_id.clone(),
        close_type: match record.close_type {
            CloseType::Coop => CloseTypeView::Coop,
            CloseType::Force => CloseTypeView::Force,
            CloseType::Unknown => CloseTypeView::Unknown,
        },
        initiator: match record.initiator {
            Initiator::Local => CloseInitiatorView::Local,
            Initiator::Remote => CloseInitiatorView::Remote,
            Initiator::Unknown => CloseInitiatorView::Unknown,
        },
        closure_reason: record.closure_reason.clone(),
        status: derive_close_status(record, tip),
        expected_amount_sats: record.expected_amount_sats,
        timelock_blocks: record.timelock_blocks,
        claimable_at_height: record.claimable_at_height,
        current_height: tip,
        created_at_ms: record.created_at_ms,
        completed_at_ms: record.completed_at_ms,
        resolved_unverified: record.resolution == Some(Resolution::Unverified)
            && record.completed_at_ms.is_some(),
        funding_txid: record.funding_txo.as_ref().map(|txo| txo.txid.clone()),
        funding_vout: record.funding_txo.as_ref().map(|txo| txo.vout),
        txs: record
            .txs
            .iter()
            .map(|tx| CloseTxView {
                txid: tx.txid.clone(),
                role: match tx.role {
                    CloseTxRole::Closing => CloseTxRoleView::Closing,
                    CloseTxRole::Commitment => CloseTxRoleView::Commitment,
                    CloseTxRole::AnchorCpfp => CloseTxRoleView::FeeBump,
                    CloseTxRole::HtlcClaim => CloseTxRoleView::PaymentClaim,
                    CloseTxRole::Sweep => CloseTxRoleView::SweepToWallet,
                    CloseTxRole::Other(_) => CloseTxRoleView::Other,
                },
                fee_sats: tx.fee_sats,
                confirmed_at_height: tx.confirmed_at_height,
                confirmations: match (tip, tx.confirmed_at_height) {
                    (Some(tip), Some(height)) if tip >= height => Some(tip - height + 1),
                    _ => None,
                },
            })
            .collect(),
    }
}

/// The input family a classified send input belongs to (U6, R5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum ClassifiedKind {
    /// A validated mainnet BOLT11 invoice — dispatch via `send_bolt11`.
    Bolt11,
    /// A validated mainnet BOLT12 offer — dispatch via `pay_offer`.
    Bolt12,
    /// An unresolved BIP353 name — resolve via `resolve_input`.
    Bip353,
    /// A resolved LNURL-pay Lightning Address (`resolve_input` output only).
    Lnurl,
    /// A mainnet on-chain address — dispatch via `send_onchain`.
    Onchain,
    /// Unusable input; `error` carries the PWA's message verbatim.
    Invalid,
}

/// A classified send input, flattened for the shells (U6). Exactly the
/// fields the send screens render: amounts, description, expiry/network
/// failures as `error`, and the preserved BIP321 on-chain fallback (AE5).
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ClassifiedView {
    pub kind: ClassifiedKind,
    /// The BOLT11 string to hand back to `send_bolt11` (kind = Bolt11).
    pub bolt11: Option<String>,
    /// The invoice's payment hash as lowercase hex (kind = Bolt11 only), so a
    /// shell can match `PaymentSuccessful`/`PaymentFailed` to the dispatch it
    /// is waiting on instead of settling on whichever outcome arrives first —
    /// which lets a previously timed-out payment steal a later send's result.
    ///
    /// `None` for every other kind, BOLT12 included: an offer has no payment
    /// hash until the invoice request produces an invoice, so BOLT12 sends keep
    /// first-outcome matching in the shells.
    pub payment_hash: Option<String>,
    /// The BOLT12 offer string to hand back to `pay_offer` (kind = Bolt12).
    pub offer: Option<String>,
    /// Embedded Lightning amount; `None` means the shells collect one.
    pub amount_msat: Option<u64>,
    /// Invoice/offer description for the review screen.
    pub description: Option<String>,
    /// On-chain address (kind = Onchain).
    pub address: Option<String>,
    /// BIP321 `amount` in sats, when the on-chain arm is the target.
    pub amount_sats: Option<u64>,
    /// BIP353 name halves (kind = Bip353).
    pub bip353_user: Option<String>,
    pub bip353_domain: Option<String>,
    /// AE5: a BIP321 URI's mainnet on-chain address, preserved as the
    /// ordered fallback even when `lno`/`lightning` won the preference.
    pub onchain_fallback_address: Option<String>,
    /// The BIP321 URI's `amount` in sats regardless of the preferred arm.
    pub uri_amount_sats: Option<u64>,
    /// The PWA's classification error string, verbatim (kind = Invalid).
    pub error: Option<String>,
}

/// A resolved LNURL-pay target (U6): everything the amount screen and the
/// invoice fetch need (LUD-16 min/max, skip-amount flag, LUD-06 metadata
/// commitment).
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct LnurlPayView {
    pub user: String,
    pub domain: String,
    pub callback: String,
    pub min_sendable_msat: u64,
    pub max_sendable_msat: u64,
    /// Bounds in sats: min rounded UP, max rounded DOWN (PWA parity).
    pub min_sats: u64,
    pub max_sats: u64,
    /// `min_sats == max_sats` — fixed-amount LNURL, the shells skip the
    /// numpad and call `fetch_lnurl_invoice(min_sendable_msat)` directly.
    pub skip_amount_entry: bool,
    pub description: String,
    /// Hex sha256 of the raw LUD-06 metadata; pass back to
    /// `fetch_lnurl_invoice` for the KTD-6 `description_hash` check.
    pub expected_description_hash_hex: Option<String>,
}

/// `resolve_input`'s result: the (possibly re-)classified input, plus the
/// LNURL metadata when resolution landed on a Lightning Address.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ResolvedView {
    pub classified: ClassifiedView,
    pub lnurl: Option<LnurlPayView>,
}

fn empty_view(kind: ClassifiedKind) -> ClassifiedView {
    ClassifiedView {
        kind,
        bolt11: None,
        payment_hash: None,
        offer: None,
        amount_msat: None,
        description: None,
        address: None,
        amount_sats: None,
        bip353_user: None,
        bip353_domain: None,
        onchain_fallback_address: None,
        uri_amount_sats: None,
        error: None,
    }
}

/// Flattens a [`Classified`] into the FFI view (BIP321 wrappers flatten to
/// their preferred arm, with the fallback fields preserved).
fn classified_view(classified: &Classified) -> ClassifiedView {
    let mut view = empty_view(ClassifiedKind::Invalid);
    if let Classified::Bip321 {
        onchain_fallback,
        amount_sats,
        ..
    } = classified
    {
        view.onchain_fallback_address = onchain_fallback.clone();
        view.uri_amount_sats = *amount_sats;
    }
    match classified.effective() {
        Classified::Bolt11 {
            raw,
            amount_msat,
            description,
            payment_hash,
        } => {
            view.kind = ClassifiedKind::Bolt11;
            view.bolt11 = Some(raw.clone());
            view.amount_msat = *amount_msat;
            view.description = description.clone();
            view.payment_hash = Some(payment_hash.clone());
        }
        Classified::Bolt12 {
            raw,
            amount_msat,
            description,
        } => {
            view.kind = ClassifiedKind::Bolt12;
            view.offer = Some(raw.clone());
            view.amount_msat = *amount_msat;
            view.description = description.clone();
        }
        Classified::Bip353 { user, domain, .. } => {
            view.kind = ClassifiedKind::Bip353;
            view.bip353_user = Some(user.clone());
            view.bip353_domain = Some(domain.clone());
        }
        Classified::Lnurl { metadata, .. } => {
            view.kind = ClassifiedKind::Lnurl;
            view.description = Some(metadata.description.clone());
        }
        Classified::Onchain {
            address,
            amount_sats,
        } => {
            view.kind = ClassifiedKind::Onchain;
            view.address = Some(address.clone());
            view.amount_sats = *amount_sats;
        }
        Classified::Invalid { reason } => {
            view.kind = ClassifiedKind::Invalid;
            view.error = Some(reason.clone());
        }
        // effective() never returns the wrapper itself.
        Classified::Bip321 { .. } => unreachable!("effective() unwraps Bip321"),
    }
    view
}

fn lnurl_view(metadata: &LnurlPayMetadata) -> LnurlPayView {
    LnurlPayView {
        user: metadata.user.clone(),
        domain: metadata.domain.clone(),
        callback: metadata.callback.clone(),
        min_sendable_msat: metadata.min_sendable_msat,
        max_sendable_msat: metadata.max_sendable_msat,
        min_sats: metadata.min_sats(),
        max_sats: metadata.max_sats(),
        skip_amount_entry: metadata.skip_amount_entry(),
        description: metadata.description.clone(),
        expected_description_hash_hex: metadata
            .expected_description_hash
            .as_ref()
            .map(|hash| crate::util::hex_str(hash)),
    }
}

fn lnurl_metadata_from_view(view: &LnurlPayView) -> Result<LnurlPayMetadata, WalletError> {
    let expected_description_hash = match &view.expected_description_hash_hex {
        None => None,
        Some(hex) => {
            let bytes = parse_hex_32(hex).ok_or_else(|| WalletError::ResolveFailed {
                detail: "invalid expected_description_hash_hex".to_string(),
            })?;
            Some(bytes)
        }
    };
    Ok(LnurlPayMetadata {
        domain: view.domain.clone(),
        user: view.user.clone(),
        callback: view.callback.clone(),
        min_sendable_msat: view.min_sendable_msat,
        max_sendable_msat: view.max_sendable_msat,
        description: view.description.clone(),
        expected_description_hash,
    })
}

fn parse_hex_32(hex: &str) -> Option<[u8; 32]> {
    if hex.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
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
    /// Another client took over this seed's cloud store; the wallet is
    /// fenced (U3/KTD-3). Queries stay readable so the fenced screen can
    /// render; `start()` is refused until the user wipes and restores.
    Fenced,
    /// A mnemonic that is not a valid BIP39 English 12-word mnemonic (U1).
    InvalidMnemonic,
    /// `restore()` found no backup on VSS for the entered words (U4, F3).
    /// Local state was untouched.
    NoBackupFound,
    /// `restore()` found remote keys the backup's manifest cannot explain —
    /// restoring could drop fund-safety state, so it aborted before any
    /// write (U4).
    BackupInconsistent { detail: String },
    /// `restore()` failed for another reason (download, validation, or a
    /// local write); `detail` says where. If the failure happened after the
    /// durable marker was written, the next `start()` resumes the restore.
    RestoreFailed { detail: String },
    /// `reveal_mnemonic()` before any wallet exists.
    NoMnemonic,
    /// `event_handled()` with no event pending — an ack without a handle.
    NoPendingEvent,
    /// The LSPS2 JIT flow failed; `reason` is the same distinct reason the
    /// corresponding [`Event::Lsps2Failed`] carries.
    Lsps2 { reason: String },
    /// `jit_accept` found the quote too stale to mint a payable invoice
    /// (U7, R6: under 60 s of life after the 30 s flight margin) — request a
    /// fresh quote via `jit_quote`. Raised BEFORE any `buy`, so nothing was
    /// committed LSP-side.
    JitReQuoteRequired,
    /// `send()` with a bolt11 string that failed to parse or verify.
    InvalidInvoice { detail: String },
    /// `send()` with an invoice that is already expired.
    InvoiceExpired,
    /// `send()` with an invoice for a different network (this wallet pays
    /// mainnet invoices only); `network` names the invoice's network.
    WrongNetwork { network: String },
    /// An amountless invoice/offer sent without an amount (U6: supply
    /// `amount_msat` for amountless requests).
    AmountlessInvoice,
    /// An `amount_msat` override supplied for an invoice/offer that already
    /// carries an amount (U6).
    AmountOverrideNotAllowed,
    /// `pay_offer()` with an offer string that failed to parse or verify
    /// (U6).
    InvalidOffer { detail: String },
    /// `pay_offer()` with an offer that is already expired (U6).
    OfferExpired,
    /// `pay_offer()` with an offer for a different network (U6).
    OfferWrongNetwork,
    /// `resolve_input()`/`fetch_lnurl_invoice()` failed; `detail` is the
    /// PWA's user-facing resolution error, verbatim (U6, R5).
    ResolveFailed { detail: String },
    /// `send()` of an invoice whose payment is already pending — paying again
    /// would risk paying twice, so the original attempt owns the outcome.
    DuplicatePayment,
    /// The send attempt failed (e.g. no route); `reason` is the same distinct
    /// reason the corresponding [`Event::PaymentFailed`] carries.
    SendFailed { reason: String },
    /// An on-chain address that failed to parse at all (U8).
    InvalidAddress { detail: String },
    /// An on-chain address for a different network (U8; PWA copy).
    WrongAddressNetwork,
    /// The on-chain fee estimate exceeds the 50,000-sat ceiling (U8/KTD-9;
    /// PWA "try again later" copy).
    OnchainFeesTooHigh,
    /// A send-all cannot clear the recipient's dust floor after fees (U8;
    /// PWA copy).
    OnchainBalanceTooLow,
    /// The tx built at the broadcast boundary differs from the reviewed
    /// amounts — the R5 drift guard; shells re-render "Amounts were updated".
    OnchainAmountChanged,
    /// amount + fee + anchor reserve exceed the spendable balance (U8, R7).
    OnchainInsufficientFunds { reserve_sats: u64 },
    /// The requested amount is below the recipient script's dust floor (U8).
    OnchainAmountBelowDust { min_sats: u64 },
    /// The on-chain tx could not be built or signed (U8); `detail` says why.
    OnchainSendFailed { detail: String },
    /// A peer address that failed to parse as `pubkey@host:port` (U9);
    /// `detail` is the distinct parse failure (missing port, bad pubkey, …).
    InvalidPeerAddress { detail: String },
    /// A bare peer pubkey argument that failed to parse (U9).
    InvalidPeerPubkey,
    /// The peer dial or handshake failed/timed out (U9).
    PeerConnectFailed { detail: String },
    /// The known-peers store could not be written (U9).
    PeerPersistFailed { detail: String },
    /// `forget_peer` refused: channels with the peer are open (U9; PWA copy).
    PeerHasOpenChannels,
    /// `open_channel` below the 20,000-sat minimum (U9; PWA copy).
    ChannelAmountBelowMinimum,
    /// `open_channel` above the 16,777,215-sat maximum (U9; PWA copy).
    ChannelAmountAboveMaximum,
    /// `open_channel` amount plus the estimated funding fee exceeds the
    /// spendable balance (U9; PWA copy).
    ChannelAmountExceedsBalance,
    /// `create_channel` was rejected (U9); `detail` says why.
    ChannelOpenFailed { detail: String },
    /// No open channel has the given id (U9).
    ChannelNotFound,
    /// The cooperative or force close call failed (U9); `detail` says why.
    ChannelCloseFailed { detail: String },
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
            WalletError::Fenced => write!(
                f,
                "this wallet is active on another device; restore from backup to take over here"
            ),
            WalletError::InvalidMnemonic => write!(
                f,
                "the mnemonic is not a valid BIP39 English 12-word mnemonic"
            ),
            // The PWA's Restore.tsx error copy, verbatim (R12 copy parity).
            WalletError::NoBackupFound => write!(
                f,
                "No backup found for this wallet. Make sure you entered the correct seed phrase."
            ),
            WalletError::BackupInconsistent { detail } => {
                write!(f, "backup inconsistent: {detail}")
            }
            WalletError::RestoreFailed { detail } => write!(f, "Restore failed: {detail}"),
            WalletError::NoMnemonic => write!(f, "no wallet mnemonic exists yet"),
            WalletError::NoPendingEvent => write!(f, "no event is pending an ack"),
            WalletError::Lsps2 { reason } => write!(f, "LSPS2 request failed: {reason}"),
            // The PWA's JitQuoteFreshnessError copy, verbatim.
            WalletError::JitReQuoteRequired => {
                write!(f, "{}", Lsps2Error::QuoteExpired)
            }
            WalletError::InvalidInvoice { detail } => {
                write!(f, "invalid bolt11 invoice: {detail}")
            }
            WalletError::InvoiceExpired => write!(f, "the invoice is expired"),
            WalletError::WrongNetwork { network } => write!(
                f,
                "the invoice is for the {network} network, this wallet only pays bitcoin \
                 (mainnet) invoices"
            ),
            // The PWA's copy, verbatim (context.tsx:981).
            WalletError::AmountlessInvoice => write!(
                f,
                "Amount is required for invoices without an embedded amount"
            ),
            WalletError::AmountOverrideNotAllowed => {
                write!(f, "{}", SendError::AmountOverrideNotAllowed)
            }
            WalletError::InvalidOffer { detail } => {
                write!(f, "invalid bolt12 offer: {detail}")
            }
            WalletError::OfferExpired => write!(f, "{}", SendError::OfferExpired),
            WalletError::OfferWrongNetwork => write!(f, "{}", SendError::OfferWrongNetwork),
            // The resolution taxonomy already carries the PWA's exact
            // user-facing strings — render them untouched.
            WalletError::ResolveFailed { detail } => write!(f, "{detail}"),
            WalletError::DuplicatePayment => {
                write!(f, "a payment for this invoice is already pending")
            }
            WalletError::SendFailed { reason } => write!(f, "sending failed: {reason}"),
            // U8: the on-chain errors reuse the core engine's PWA-parity copy.
            WalletError::InvalidAddress { detail } => {
                write!(
                    f,
                    "{}",
                    OnchainSendError::InvalidAddress {
                        detail: detail.clone()
                    }
                )
            }
            WalletError::WrongAddressNetwork => write!(f, "{}", OnchainSendError::WrongNetwork),
            WalletError::OnchainFeesTooHigh => write!(f, "{}", OnchainSendError::FeeTooHigh),
            WalletError::OnchainBalanceTooLow => write!(f, "{}", OnchainSendError::BalanceTooLow),
            WalletError::OnchainAmountChanged => write!(f, "{}", OnchainSendError::DriftDetected),
            WalletError::OnchainInsufficientFunds { reserve_sats } => {
                write!(
                    f,
                    "{}",
                    OnchainSendError::InsufficientFunds {
                        reserve_sats: *reserve_sats
                    }
                )
            }
            WalletError::OnchainAmountBelowDust { min_sats } => {
                write!(
                    f,
                    "{}",
                    OnchainSendError::AmountBelowDust {
                        min_sats: *min_sats
                    }
                )
            }
            WalletError::OnchainSendFailed { detail } => {
                write!(f, "on-chain send failed: {detail}")
            }
            // U9: the channel errors reuse the core engine's PWA-parity copy.
            WalletError::InvalidPeerAddress { detail } => write!(f, "{detail}"),
            WalletError::InvalidPeerPubkey => write!(f, "{}", ChannelsError::InvalidPubkey),
            WalletError::PeerConnectFailed { detail } => {
                write!(
                    f,
                    "{}",
                    ChannelsError::ConnectFailed {
                        detail: detail.clone()
                    }
                )
            }
            WalletError::PeerPersistFailed { detail } => {
                write!(
                    f,
                    "{}",
                    ChannelsError::PersistFailed {
                        detail: detail.clone()
                    }
                )
            }
            WalletError::PeerHasOpenChannels => {
                write!(f, "{}", ChannelsError::PeerHasOpenChannels)
            }
            WalletError::ChannelAmountBelowMinimum => {
                write!(f, "{}", ChannelsError::AmountBelowMinimum)
            }
            WalletError::ChannelAmountAboveMaximum => {
                write!(f, "{}", ChannelsError::AmountAboveMaximum)
            }
            WalletError::ChannelAmountExceedsBalance => {
                write!(f, "{}", ChannelsError::AmountExceedsBalance)
            }
            WalletError::ChannelOpenFailed { detail } => {
                write!(
                    f,
                    "{}",
                    ChannelsError::OpenFailed {
                        detail: detail.clone()
                    }
                )
            }
            WalletError::ChannelNotFound => write!(f, "{}", ChannelsError::ChannelNotFound),
            WalletError::ChannelCloseFailed { detail } => {
                write!(
                    f,
                    "{}",
                    ChannelsError::CloseFailed {
                        detail: detail.clone()
                    }
                )
            }
        }
    }
}

impl From<ChannelsError> for WalletError {
    fn from(error: ChannelsError) -> Self {
        match error {
            ChannelsError::NotRunning => WalletError::NotRunning,
            ChannelsError::InvalidAddress(parse_error) => WalletError::InvalidPeerAddress {
                detail: parse_error.to_string(),
            },
            ChannelsError::InvalidPubkey => WalletError::InvalidPeerPubkey,
            ChannelsError::ConnectFailed { detail } => WalletError::PeerConnectFailed { detail },
            ChannelsError::PersistFailed { detail } => WalletError::PeerPersistFailed { detail },
            ChannelsError::PeerHasOpenChannels => WalletError::PeerHasOpenChannels,
            ChannelsError::AmountBelowMinimum => WalletError::ChannelAmountBelowMinimum,
            ChannelsError::AmountAboveMaximum => WalletError::ChannelAmountAboveMaximum,
            ChannelsError::AmountExceedsBalance => WalletError::ChannelAmountExceedsBalance,
            ChannelsError::OpenFailed { detail } => WalletError::ChannelOpenFailed { detail },
            ChannelsError::ChannelNotFound => WalletError::ChannelNotFound,
            ChannelsError::CloseFailed { detail } => WalletError::ChannelCloseFailed { detail },
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
            BuildError::Fenced => WalletError::Fenced,
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

impl From<RestoreError> for WalletError {
    fn from(error: RestoreError) -> Self {
        match error {
            RestoreError::NodeRunning => WalletError::AlreadyRunning,
            RestoreError::InvalidMnemonic => WalletError::InvalidMnemonic,
            RestoreError::NoBackupFound => WalletError::NoBackupFound,
            RestoreError::BackupInconsistent { detail } => {
                WalletError::BackupInconsistent { detail }
            }
            other @ (RestoreError::VssDisabled
            | RestoreError::Setup { .. }
            | RestoreError::DownloadFailed { .. }
            | RestoreError::ValidationFailed { .. }
            | RestoreError::LocalWriteFailed { .. }
            | RestoreError::Interrupted) => WalletError::RestoreFailed {
                detail: other.to_string(),
            },
        }
    }
}

impl From<OnchainSendError> for WalletError {
    fn from(error: OnchainSendError) -> Self {
        match error {
            OnchainSendError::NotRunning => WalletError::NotRunning,
            OnchainSendError::InvalidAddress { detail } => WalletError::InvalidAddress { detail },
            OnchainSendError::WrongNetwork => WalletError::WrongAddressNetwork,
            OnchainSendError::FeeTooHigh => WalletError::OnchainFeesTooHigh,
            OnchainSendError::BalanceTooLow => WalletError::OnchainBalanceTooLow,
            OnchainSendError::DriftDetected => WalletError::OnchainAmountChanged,
            OnchainSendError::InsufficientFunds { reserve_sats } => {
                WalletError::OnchainInsufficientFunds { reserve_sats }
            }
            OnchainSendError::AmountBelowDust { min_sats } => {
                WalletError::OnchainAmountBelowDust { min_sats }
            }
            OnchainSendError::BuildFailed { detail }
            | OnchainSendError::SigningFailed { detail } => {
                WalletError::OnchainSendFailed { detail }
            }
        }
    }
}

impl From<Lsps2Error> for WalletError {
    fn from(error: Lsps2Error) -> Self {
        match error {
            Lsps2Error::NotRunning => WalletError::NotRunning,
            // The typed re-quote signal (U7, R6) stays distinguishable so
            // the shells re-quote instead of rendering a generic failure.
            Lsps2Error::QuoteExpired => WalletError::JitReQuoteRequired,
            other => WalletError::Lsps2 {
                reason: other.to_string(),
            },
        }
    }
}

impl From<ReceiveError> for WalletError {
    fn from(error: ReceiveError) -> Self {
        match error {
            ReceiveError::NotRunning => WalletError::NotRunning,
            // The address reveal is an on-chain wallet operation; its
            // failure maps like next_receive_address's does.
            other @ (ReceiveError::AddressUnavailable { .. }
            | ReceiveError::InvoiceCreationFailed) => WalletError::OnchainSendFailed {
                detail: other.to_string(),
            },
        }
    }
}

impl From<ResolveError> for WalletError {
    fn from(error: ResolveError) -> Self {
        // The Display strings ARE the PWA's user-facing copy (send.rs).
        WalletError::ResolveFailed {
            detail: error.to_string(),
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
            SendError::AmountOverrideNotAllowed => WalletError::AmountOverrideNotAllowed,
            SendError::InvalidOffer(detail) => WalletError::InvalidOffer { detail },
            SendError::OfferExpired => WalletError::OfferExpired,
            SendError::OfferWrongNetwork => WalletError::OfferWrongNetwork,
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

/// The unified BIP321 receive URI (U7, R6), byte-exact with the PWA's
/// `buildBip321Uri`: `bitcoin:{ADDRESS uppercased}` with `amount` (BTC,
/// fixed 8 decimals, only when > 0) then `lightning={invoice}`. This is the
/// copy/share form; uppercase the WHOLE string for QR alphanumeric mode
/// (as `ReceiveBundle::qr_value` already does). Pure — usable while the
/// node is stopped (e.g. re-composing the URI around a fresh JIT invoice).
#[uniffi::export]
pub fn build_bip321_uri(
    address: String,
    amount_sats: Option<u64>,
    invoice: Option<String>,
) -> String {
    crate::receive::build_bip321_uri(&address, amount_sats, invoice.as_deref())
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
            .map_err(WalletError::from)
    }

    /// Everything the receive screen renders in one call (U7, R6): on-chain
    /// address, standard invoice when inbound capacity covers `amount_msat`
    /// (amountless allowed), the unified BIP321 URI (copy + QR forms), the
    /// persisted BOLT12 offer when a usable channel exists, the session JIT
    /// floor, and the `needs_jit` capacity decision. When `needs_jit` is
    /// `true` with an amount, drive [`Wallet::jit_quote`] →
    /// [`Wallet::jit_accept`]; amountless with no capacity renders the
    /// on-chain-only QR (the PWA's Receive flow). Never blocks on the
    /// network. Requires a running node.
    pub fn receive_bundle(&self, amount_msat: Option<u64>) -> Result<ReceiveBundle, WalletError> {
        self.node
            .receive_bundle(amount_msat)
            .map_err(WalletError::from)
    }

    /// JIT phase A (U7, F2): fetch a quote — fee, net amount, validity,
    /// freshness, and a single-use token — WITHOUT committing anything
    /// LSP-side, so the review screen can disclose the setup fee first.
    /// Below-floor amounts fail typed here, before any `buy` (AE4).
    /// Blocking (LSP round-trip): call from a background dispatcher.
    pub fn jit_quote(&self, amount_msat: u64) -> Result<JitQuote, WalletError> {
        self.node.jit_quote(amount_msat).map_err(WalletError::from)
    }

    /// JIT phase B (U7, F2): commit the reviewed quote — buy, then mint the
    /// wrapped invoice with its expiry clamped to the quote's remaining
    /// validity (R6). A quote with under 60 s of payable life left fails
    /// with [`WalletError::JitReQuoteRequired`] BEFORE the buy — request a
    /// fresh quote. The invoice also arrives as [`Event::InvoiceReady`] with
    /// the clamped expiry; failures as [`Event::Lsps2Failed`]. Blocking:
    /// call from a background dispatcher.
    pub fn jit_accept(
        &self,
        quote_token: u64,
        amount_msat: u64,
    ) -> Result<JitInvoice, WalletError> {
        self.node
            .jit_accept(quote_token, amount_msat)
            .map_err(WalletError::from)
    }

    /// The JIT numpad floor in sats (U7, R6, AE4): one amountless
    /// `lsps2.get_info` per receive session — pass `refresh = true` on
    /// entering the receive screen to start a session, `false` afterwards to
    /// read the cached value. NEVER errors: failures, empty menus, and a
    /// stopped node all degrade to the static 3,000-sat floor. Blocking on a
    /// fetch: call from a background dispatcher.
    pub fn min_receive_sats(&self, refresh: bool) -> u64 {
        self.node.min_receive_sats(refresh)
    }

    /// The persistent BOLT12 offer (U7, R6): the persisted one when it
    /// exists, else created via LDK's offer builder (mainnet chain,
    /// description `zinqq wallet`) with the PWA's 3/6/12/24/48 s retry
    /// backoff — blinded paths need the RGS-synced graph — and persisted
    /// under a stable key so every session serves the same offer. `None`
    /// when stopped or when creation keeps failing: offer problems NEVER
    /// block receive. Blocking (up to the retry schedule): call from a
    /// background dispatcher.
    pub fn get_or_create_offer(&self) -> Option<String> {
        self.node.get_or_create_offer()
    }

    /// Whether the BOLT12 offer pager page should render (U7, R6): a
    /// persisted offer exists AND at least one channel is usable. `false`
    /// while stopped.
    pub fn offer_available(&self) -> bool {
        self.node.offer_available()
    }

    /// Pays a fixed-amount mainnet BOLT11 invoice (compat shim for the
    /// spike-era surface; new callers use [`Wallet::send_bolt11`]).
    pub fn send(&self, bolt11: String) -> Result<(), WalletError> {
        self.send_bolt11(bolt11, None)
    }

    /// Pays a mainnet BOLT11 invoice; the outcome arrives as
    /// [`Event::PaymentSuccessful`] / [`Event::PaymentFailed`]. Blocking
    /// (route computation): call from a background dispatcher.
    ///
    /// `amount_msat` is the U6 amount override: REQUIRED for amountless
    /// invoices, REJECTED ([`WalletError::AmountOverrideNotAllowed`]) when
    /// the invoice already carries an amount. Idempotent across restarts
    /// (U5): the payment id is derived from the invoice's payment hash, so
    /// re-sending an in-flight invoice fails with
    /// [`WalletError::DuplicatePayment`] instead of paying twice. Invalid
    /// invoices (malformed / expired / wrong network) each fail with a
    /// distinct typed error before anything is attempted.
    pub fn send_bolt11(&self, bolt11: String, amount_msat: Option<u64>) -> Result<(), WalletError> {
        self.node
            .send_payment(&bolt11, amount_msat)
            .map_err(WalletError::from)
    }

    /// Pays a mainnet BOLT12 offer (U6, R5): 32-byte random payment id,
    /// optional payer note on the invoice request, LSP pre-connect for the
    /// onion transport, retry ×3. `amount_msat` follows the same override
    /// matrix as [`Wallet::send_bolt11`]. The outcome arrives as
    /// [`Event::PaymentSuccessful`] / [`Event::PaymentFailed`] and settles
    /// the pending history row by payment id. Blocking (LSP dial): call from
    /// a background dispatcher.
    pub fn pay_offer(
        &self,
        offer: String,
        amount_msat: Option<u64>,
        payer_note: Option<String>,
    ) -> Result<(), WalletError> {
        self.node
            .pay_offer(&offer, amount_msat, payer_note)
            .map_err(WalletError::from)
    }

    /// Classifies a send input (U6, R5): the PWA's dispatch order, network
    /// and expiry checks, and error strings, verbatim. Pure and synchronous;
    /// works with the node stopped.
    pub fn classify_input(&self, input: String) -> ClassifiedView {
        classified_view(&send::classify(&input))
    }

    /// Classifies AND resolves a send input (U6, R5): BIP353 names resolve
    /// over DNSSEC-verified DoH (5 s budget) with an LNURL-pay fallback on a
    /// miss (fresh 5 s budget); other inputs pass through classification
    /// unchanged. The returned view is directly dispatchable except for
    /// LNURL, which carries min/max bounds and the skip-amount flag for the
    /// amount screen. Async: the network work runs on the core runtime.
    pub async fn resolve_input(&self, input: String) -> Result<ResolvedView, WalletError> {
        let task = crate::runtime().spawn(async move {
            let classified = send::classify(&input);
            send::resolve(classified, &HttpNameResolver::new(), unix_now()).await
        });
        let resolved = task
            .await
            .expect("wallet-core runtime task panicked")
            .map_err(WalletError::from)?;
        let lnurl = match &resolved {
            Classified::Lnurl { metadata, .. } => Some(lnurl_view(metadata)),
            _ => None,
        };
        Ok(ResolvedView {
            classified: classified_view(&resolved),
            lnurl,
        })
    }

    /// Fetches the final BOLT11 invoice from a resolved LNURL-pay target for
    /// `amount_msat` (U6): bounds-checked against the LUD-16 window, then
    /// validated per KTD-6 (re-classified, amount match, `description_hash`
    /// commitment). Returns the invoice's classified view — hand its
    /// `bolt11` to [`Wallet::send_bolt11`] with NO amount override.
    pub async fn fetch_lnurl_invoice(
        &self,
        lnurl: LnurlPayView,
        amount_msat: u64,
    ) -> Result<ClassifiedView, WalletError> {
        let metadata = lnurl_metadata_from_view(&lnurl)?;
        let task = crate::runtime().spawn(async move {
            send::fetch_lnurl_invoice(&HttpNameResolver::new(), &metadata, amount_msat, unix_now())
                .await
        });
        let classified = task
            .await
            .expect("wallet-core runtime task panicked")
            .map_err(WalletError::from)?;
        Ok(classified_view(&classified))
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

    /// Replaces the current wallet with the one the entered 12 words back up
    /// on VSS (U4, F3: the destructive confirm lives in the shells). Valid
    /// only while stopped ([`WalletError::AlreadyRunning`] otherwise).
    /// Blocking (backup downloads): call from a background dispatcher;
    /// progress arrives as [`Event::RestoreProgress`] with the PWA's exact
    /// step copy. On success the node is restartable via `start()`.
    pub fn restore(&self, mnemonic: String) -> Result<(), WalletError> {
        self.node.restore(&mnemonic).map_err(WalletError::from)
    }

    /// The stored 12 words for the Backup screen (U4, R1 reveal half — the
    /// 60-second auto-hide is UI policy in the shells). Readable while
    /// stopped; [`WalletError::NoMnemonic`] before the first start.
    pub fn reveal_mnemonic(&self) -> Result<String, WalletError> {
        self.node.reveal_mnemonic().ok_or(WalletError::NoMnemonic)
    }

    /// Current balances; requires a running node. On-chain changes announce
    /// themselves with [`Event::OnchainStateChanged`] (the background bdk sync
    /// is the only thing that observes them); re-read on that event.
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

    /// Fee estimate for an exact-amount on-chain send (U8, R7): builds the
    /// tx at the 6-block rate (clamped >= 2 sat/vB, KTD-9) without
    /// broadcasting; a fee above 50,000 sats is
    /// [`WalletError::OnchainFeesTooHigh`]. Requires a running node.
    pub fn estimate_onchain_fee(
        &self,
        address: String,
        amount_sats: u64,
    ) -> Result<FeeEstimate, WalletError> {
        self.node
            .estimate_onchain_fee(&address, amount_sats)
            .map_err(WalletError::from)
    }

    /// Max-sendable estimate for the given recipient (U8, R7): the drain
    /// amount after fees and the 10,000-sat anchor reserve (withheld iff at
    /// least one channel is open). Requires a running node.
    pub fn estimate_max_sendable(&self, address: String) -> Result<MaxSendEstimate, WalletError> {
        self.node
            .estimate_max_sendable(&address)
            .map_err(WalletError::from)
    }

    /// Exact-amount on-chain send (U8, R7): amount + fee + reserve must fit
    /// in the spendable balance; `expected_amount_sats`/`expected_fee_sats`
    /// are the review-screen values, re-verified at the broadcast boundary —
    /// any change is [`WalletError::OnchainAmountChanged`] (R5 drift guard).
    /// Returns the txid; the persist-first broadcaster owns delivery
    /// (U12/KTD-9). On-chain history rows arrive via `list_activity` from
    /// the wallet's transactions, exactly like the PWA.
    pub fn send_onchain(
        &self,
        address: String,
        amount_sats: u64,
        expected_amount_sats: u64,
        expected_fee_sats: u64,
    ) -> Result<String, WalletError> {
        self.node
            .send_onchain(
                &address,
                amount_sats,
                expected_amount_sats,
                expected_fee_sats,
            )
            .map_err(WalletError::from)
    }

    /// On-chain send-max (U8, AE6): drains fully at zero channels; with
    /// channels exactly 10,000 sats remain as an explicit reserve output.
    /// The same drift guard applies. Returns the txid.
    pub fn send_onchain_max(
        &self,
        address: String,
        expected_amount_sats: u64,
        expected_fee_sats: u64,
    ) -> Result<String, WalletError> {
        self.node
            .send_onchain_max(&address, expected_amount_sats, expected_fee_sats)
            .map_err(WalletError::from)
    }

    /// Next unused on-chain receive address (U8): revealed on the external
    /// keychain with the changeset persisted, so a restart keeps the index.
    /// Requires a running node.
    pub fn next_receive_address(&self) -> Result<String, WalletError> {
        self.node.next_receive_address().map_err(WalletError::from)
    }

    /// Connects to a `pubkey@host:port` peer and saves it as a known peer
    /// (U9, R10 — the PWA's `connectToPeer`). Blocking (dial + BOLT8
    /// handshake, 15 s budget): call from a background dispatcher. Returns
    /// the peer's pubkey hex. Requires a running node.
    pub fn connect_peer(&self, address: String) -> Result<String, WalletError> {
        self.node.connect_peer(&address).map_err(WalletError::from)
    }

    /// Disconnects a peer's socket (U9). The peer stays saved: the reconnect
    /// loop keeps dialing known peers.
    pub fn disconnect_peer(&self, pubkey: String) -> Result<(), WalletError> {
        self.node
            .disconnect_peer(&pubkey)
            .map_err(WalletError::from)
    }

    /// Removes a saved peer (U9, R10). Fails with
    /// [`WalletError::PeerHasOpenChannels`] while any channel with the peer
    /// is open (the PWA's `forgetPeer` guard).
    pub fn forget_peer(&self, pubkey: String) -> Result<(), WalletError> {
        self.node.forget_peer(&pubkey).map_err(WalletError::from)
    }

    /// The Peers screen's rows (U9, R10): the union of saved and connected
    /// peers with per-peer channel counts, connected first. Requires a
    /// running node.
    pub fn list_peers(&self) -> Result<Vec<PeerView>, WalletError> {
        self.node.list_peers().map_err(WalletError::from)
    }

    /// Every channel with its state label (Active/Ready/Pending/Closing),
    /// capacities, reserve, usable flag, and in-flight HTLC count (U9, R10).
    /// Requires a running node.
    pub fn list_channels(&self) -> Result<Vec<ChannelView>, WalletError> {
        self.node.list_channels().map_err(WalletError::from)
    }

    /// The open-channel review numbers (U9): the 6-block sat/vB rate and
    /// `rate × 140 vB` (the PWA's `OpenChannel` estimate). Requires a
    /// running node.
    pub fn estimate_open_fee(&self) -> Result<OpenFeeEstimate, WalletError> {
        self.node.estimate_open_fee().map_err(WalletError::from)
    }

    /// Opens a channel to `pubkey@host:port` (U9, R10): bounds
    /// 20,000–16,777,215 sats, balance gate at amount + estimated fee,
    /// connect-if-needed (persisting the known peer), then LDK
    /// `create_channel`. Blocking: call from a background dispatcher.
    /// Returns the TEMPORARY channel id hex; progress arrives as
    /// [`Event::ChannelPending`] / [`Event::ChannelReady`], and the funding
    /// tx is persisted before LDK is notified and broadcast only on LDK's
    /// broadcast-safe signal (fund safety).
    pub fn open_channel(
        &self,
        peer_address: String,
        amount_sats: u64,
    ) -> Result<String, WalletError> {
        self.node
            .open_channel(&peer_address, amount_sats)
            .map_err(WalletError::from)
    }

    /// Closes a channel (U9, R10): cooperative by default, unilateral
    /// (`force_close_broadcasting_latest_txn`) when `force`. The outcome
    /// arrives as [`Event::ChannelClosed`].
    pub fn close_channel(&self, channel_id: String, force: bool) -> Result<(), WalletError> {
        self.node
            .close_channel(&channel_id, force)
            .map_err(WalletError::from)
    }

    /// The informational pre-close estimate (U9, R10): every field
    /// independently nullable, and NEVER an error — a stopped node or
    /// unknown channel returns the all-`None` estimate so the close screen
    /// always renders (the PWA's `estimateClose` contract).
    pub fn estimate_close(&self, channel_id: String) -> CloseEstimate {
        self.node.estimate_close(&channel_id)
    }

    /// The force-close recovery state (U10, R9), `None` when no recovery is
    /// active. Readable while stopped (local-first store) so the banner can
    /// render immediately at startup. Changes arrive as
    /// [`Event::RecoveryStateChanged`]; re-read on every one.
    pub fn recovery_state(&self) -> Option<RecoveryStateView> {
        self.node.recovery_state().map(|state| RecoveryStateView {
            status: match state.status {
                crate::recovery::RecoveryStatus::NeedsRecovery => RecoveryStatusView::NeedsRecovery,
                crate::recovery::RecoveryStatus::SweepConfirmed => {
                    RecoveryStatusView::SweepConfirmed
                }
            },
            stuck_balance_sat: state.stuck_balance_sat,
            deposit_address: state.deposit_address,
            deposit_needed_sat: state.deposit_needed_sat,
            channel_ids: state.channel_ids,
            created_at_ms: state.created_at,
            updated_at_ms: state.updated_at,
        })
    }

    /// Durably dismiss the recovery success banner (U14/U19, R9). No-op
    /// unless the recovery state is `SweepConfirmed` — an active recovery is
    /// chain-truth-owned and not user-dismissible.
    pub fn dismiss_recovery(&self) {
        self.node.dismiss_recovery();
    }

    /// Outputs still waiting to sweep (U11, R8), `None` when nothing is
    /// pending. Readable while stopped (the store owns its own handle).
    pub fn pending_sweep(&self) -> Option<PendingSweepView> {
        self.node.pending_sweep().map(|info| PendingSweepView {
            entry_count: info.entry_count,
            descriptor_count: info.descriptor_count,
            pending_sats: info.pending_sats,
            has_unknown_value: info.has_unknown_value,
            last_attempt_failed: info.last_attempt_failed,
            needs_onchain_funds: info.needs_onchain_funds,
            shortfall_sats: info.shortfall_sats,
        })
    }

    /// One close record for the detail screen (U10, R9): the derived status
    /// label plus per-tx roles (closing / commitment / fee bump / payment
    /// claim / sweep-to-wallet) with confirmation counts against the
    /// last-known tip. The activity LIST stays `list_activity` (one row per
    /// close, KTD-7). Readable while stopped.
    pub fn close_detail(&self, channel_id: String) -> Option<CloseRecordView> {
        self.node
            .close_record_with_tip(&channel_id)
            .map(|(record, tip)| close_record_view(&record, tip))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr as _;
    use std::time::Duration;

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

    /// U6: the FFI view flattens a BIP321 wrapper to its preferred arm while
    /// preserving the ordered on-chain fallback (AE5) and the URI amount.
    #[test]
    fn classified_view_flattens_bip321_and_preserves_the_fallback() {
        let classified = Classified::Bip321 {
            preferred: Box::new(Classified::Bolt12 {
                raw: "lno1abc".to_string(),
                amount_msat: Some(25_000),
                description: Some("offer".to_string()),
            }),
            onchain_fallback: Some("bc1qexample".to_string()),
            amount_sats: Some(100_000),
        };
        let view = classified_view(&classified);
        assert_eq!(view.kind, ClassifiedKind::Bolt12);
        assert_eq!(view.offer.as_deref(), Some("lno1abc"));
        assert_eq!(view.amount_msat, Some(25_000));
        assert_eq!(
            view.onchain_fallback_address.as_deref(),
            Some("bc1qexample")
        );
        assert_eq!(view.uri_amount_sats, Some(100_000));
        assert_eq!(view.bolt11, None);
        assert_eq!(view.error, None);
    }

    #[test]
    fn classified_view_carries_the_pwa_error_string_verbatim() {
        let view = classified_view(&crate::send::classify("definitely not a payment"));
        assert_eq!(view.kind, ClassifiedKind::Invalid);
        assert_eq!(view.error.as_deref(), Some("Unrecognized payment format"));
    }

    #[test]
    fn classified_view_maps_bip353_and_onchain_fields() {
        let view = classified_view(&crate::send::classify("alice@example.com"));
        assert_eq!(view.kind, ClassifiedKind::Bip353);
        assert_eq!(view.bip353_user.as_deref(), Some("alice"));
        assert_eq!(view.bip353_domain.as_deref(), Some("example.com"));

        let view = classified_view(&crate::send::classify(
            "bc1qar0srrr7xfkvy5l643lydnw9re59gtzzwf5mdq",
        ));
        assert_eq!(view.kind, ClassifiedKind::Onchain);
        assert_eq!(
            view.address.as_deref(),
            Some("bc1qar0srrr7xfkvy5l643lydnw9re59gtzzwf5mdq")
        );
    }

    /// A fixed mainnet BOLT11 invoice built at `CLASSIFY_NOW` whose payment
    /// hash is the sha256 of a known preimage.
    fn fixed_bolt11() -> String {
        use bitcoin::hashes::{sha256, Hash as _};
        use bitcoin::secp256k1::{Secp256k1, SecretKey};
        use lightning_invoice::{Currency, InvoiceBuilder, PaymentSecret};

        let secret = SecretKey::from_slice(&[0x3c; 32]).unwrap();
        InvoiceBuilder::new(Currency::Bitcoin)
            .description("payment hash pin".to_string())
            .payment_hash(sha256::Hash::hash(PREIMAGE))
            .payment_secret(PaymentSecret([0x22; 32]))
            .duration_since_epoch(Duration::from_secs(CLASSIFY_NOW))
            .min_final_cltv_expiry_delta(144)
            .expiry_time(Duration::from_secs(3_600))
            .amount_milli_satoshis(21_000)
            .build_signed(|hash| Secp256k1::new().sign_ecdsa_recoverable(hash, &secret))
            .unwrap()
            .to_string()
    }

    const PREIMAGE: &[u8] = b"zinqq-kmp classified-view payment hash preimage";
    const CLASSIFY_NOW: u64 = 1_753_000_000;

    /// The shells need THEIR dispatch's payment hash to match the public
    /// `PaymentSuccessful`/`PaymentFailed` events to the send they are waiting
    /// on; the classified view is where they learn it.
    #[test]
    fn classified_view_exposes_the_bolt11_payment_hash_as_lowercase_hex() {
        use bitcoin::hashes::{sha256, Hash as _};
        use lightning_invoice::Bolt11Invoice;

        let raw = fixed_bolt11();
        let view = classified_view(&crate::send::classify_at(
            &raw,
            Duration::from_secs(CLASSIFY_NOW + 60),
        ));
        assert_eq!(view.kind, ClassifiedKind::Bolt11);

        // Independent computation 1: the preimage's sha256.
        let expected = crate::util::hex_str(sha256::Hash::hash(PREIMAGE).as_byte_array());
        assert_eq!(view.payment_hash.as_deref(), Some(expected.as_str()));
        // Independent computation 2: re-decoding the invoice string.
        assert_eq!(
            view.payment_hash.as_deref(),
            Some(
                crate::util::hex_str(
                    Bolt11Invoice::from_str(&raw)
                        .unwrap()
                        .payment_hash()
                        .as_byte_array()
                )
                .as_str()
            )
        );
        assert_eq!(
            view.payment_hash.as_deref().unwrap(),
            view.payment_hash.as_deref().unwrap().to_lowercase(),
            "the hex is lowercase, matching the event payload format"
        );

        // A BIP321 URI whose preferred arm is the invoice carries it too.
        let uri_view = classified_view(&crate::send::classify_at(
            &format!("bitcoin:bc1qar0srrr7xfkvy5l643lydnw9re59gtzzwf5mdq?lightning={raw}"),
            Duration::from_secs(CLASSIFY_NOW + 60),
        ));
        assert_eq!(uri_view.kind, ClassifiedKind::Bolt11);
        assert_eq!(uri_view.payment_hash.as_deref(), Some(expected.as_str()));
    }

    /// Every non-BOLT11 kind leaves `payment_hash` unset — a BOLT12 offer has
    /// no payment hash before the invoice request, so those sends keep
    /// first-outcome matching in the shells.
    #[test]
    fn classified_view_has_no_payment_hash_for_offers_or_onchain() {
        use bitcoin::secp256k1::{PublicKey, Secp256k1, SecretKey};
        use lightning::offers::offer::OfferBuilder;

        let secp = Secp256k1::new();
        let signing_key =
            PublicKey::from_secret_key(&secp, &SecretKey::from_slice(&[0x3c; 32]).unwrap());
        let offer = OfferBuilder::new(signing_key)
            .description("test offer".to_string())
            .amount_msats(21_000)
            .build()
            .unwrap()
            .to_string();
        let view = classified_view(&crate::send::classify_at(
            &offer,
            Duration::from_secs(CLASSIFY_NOW),
        ));
        assert_eq!(view.kind, ClassifiedKind::Bolt12);
        assert_eq!(
            view.payment_hash, None,
            "an offer has no payment hash until the invoice request"
        );

        let view = classified_view(&crate::send::classify_at(
            "bc1qar0srrr7xfkvy5l643lydnw9re59gtzzwf5mdq",
            Duration::from_secs(CLASSIFY_NOW),
        ));
        assert_eq!(view.kind, ClassifiedKind::Onchain);
        assert_eq!(view.payment_hash, None);

        let view = classified_view(&crate::send::classify_at(
            "alice@example.com",
            Duration::from_secs(CLASSIFY_NOW),
        ));
        assert_eq!(view.kind, ClassifiedKind::Bip353);
        assert_eq!(view.payment_hash, None);

        let view = classified_view(&crate::send::classify_at(
            "definitely not a payment",
            Duration::from_secs(CLASSIFY_NOW),
        ));
        assert_eq!(view.kind, ClassifiedKind::Invalid);
        assert_eq!(view.payment_hash, None);
    }

    /// U6: the LNURL view round-trips through the FFI record (hash included)
    /// and carries the PWA's ceil/floor bounds and skip-amount flag.
    #[test]
    fn lnurl_view_round_trips_and_carries_the_skip_flag() {
        let metadata = LnurlPayMetadata {
            domain: "example.com".to_string(),
            user: "alice".to_string(),
            callback: "https://example.com/cb".to_string(),
            min_sendable_msat: 1_001,
            max_sendable_msat: 2_999,
            description: "Pay alice".to_string(),
            expected_description_hash: Some([0xab; 32]),
        };
        let view = lnurl_view(&metadata);
        assert_eq!(view.min_sats, 2, "ceil");
        assert_eq!(view.max_sats, 2, "floor");
        assert!(view.skip_amount_entry);
        assert_eq!(
            view.expected_description_hash_hex.as_deref(),
            Some("ab".repeat(32).as_str())
        );
        assert_eq!(lnurl_metadata_from_view(&view).unwrap(), metadata);

        let mut bad = view;
        bad.expected_description_hash_hex = Some("zz".to_string());
        assert!(matches!(
            lnurl_metadata_from_view(&bad),
            Err(WalletError::ResolveFailed { .. })
        ));
    }

    /// U6: resolution errors surface with the PWA string as the message.
    #[test]
    fn resolve_errors_map_to_resolve_failed_with_the_pwa_string() {
        let err = WalletError::from(ResolveError::NotFound {
            raw: "alice@example.com".to_string(),
        });
        assert_eq!(
            err.to_string(),
            "No Lightning Address or BIP 353 record found for alice@example.com"
        );
        let err = WalletError::from(ResolveError::CallbackDomainMismatch);
        assert_eq!(
            err.to_string(),
            "Lightning Address callback domain mismatch"
        );
    }

    /// U7: the LSPS2 error mapping keeps the re-quote signal typed —
    /// `QuoteExpired` becomes `JitReQuoteRequired` (the PWA freshness copy
    /// verbatim) while other failures fold into `Lsps2 { reason }` with
    /// their distinct reasons.
    #[test]
    fn lsps2_errors_map_with_a_distinct_requote_signal() {
        assert_eq!(
            WalletError::from(Lsps2Error::QuoteExpired),
            WalletError::JitReQuoteRequired
        );
        assert_eq!(
            WalletError::JitReQuoteRequired.to_string(),
            "Fee quote expired, please try again"
        );
        assert_eq!(
            WalletError::from(Lsps2Error::NotRunning),
            WalletError::NotRunning
        );
        assert_eq!(
            WalletError::from(Lsps2Error::QuoteNotFound),
            WalletError::Lsps2 {
                reason: Lsps2Error::QuoteNotFound.to_string()
            }
        );
    }

    /// U7: the FFI BIP321 export is byte-exact with the PWA's copy form.
    #[test]
    fn build_bip321_uri_export_matches_the_pwa_copy_form() {
        assert_eq!(
            build_bip321_uri(
                "bc1qw508d6qejxtdg4y5r3zarvary0c5xw7kv8f3t4".to_string(),
                Some(3_000),
                Some("lnbc30u1invoice".to_string()),
            ),
            "bitcoin:BC1QW508D6QEJXTDG4Y5R3ZARVARY0C5XW7KV8F3T4?amount=0.00003000&lightning=lnbc30u1invoice"
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
