//! Channels & peers management API (U9, R10; cites KTD-10).
//!
//! Mirrors the PWA's Peers/OpenChannel/CloseChannel behaviors:
//! - `parse_peer_address` — `pubkey@host:port` with the PWA's validation
//!   order and messages (`zinq/src/ldk/peers/peer-connection.ts:149-175`).
//!   The core dials `SocketAddr`s only (U12 decision: hostnames are a typed
//!   error, consistent with the known-peers reconnect skip).
//! - Open-channel bounds 20,000–16,777,215 sats and the 6-block × 140 vB fee
//!   estimate (`zinq/src/pages/OpenChannel.tsx:29-34,68-72,97-98`).
//! - The funding flow's write order (`zinq/src/ldk/traits/event-handler.ts`):
//!   the funding tx is persisted BEFORE LDK is notified, the broadcast is
//!   driven by `FundingTxBroadcastSafe`, and `DiscardFunding` cleans up via
//!   the real→temporary channel-id map recorded at `ChannelPending`.
//! - `forget_peer` refuses while any channel with that peer is open
//!   (`zinq/src/ldk/context.tsx:852-868`); the last channel closing
//!   auto-forgets the peer (`zinq/src/ldk/context.tsx:1233-1244`).
//! - `estimate_close` is informational and NEVER gates a close: every field
//!   is independently nullable and any ambiguity degrades to `None`
//!   (`zinq/src/ldk/close-records/estimate.ts`).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::net::SocketAddr;
use std::str::FromStr as _;
use std::sync::Arc;
use std::time::Duration;

use bitcoin::secp256k1::rand::rngs::OsRng;
use bitcoin::secp256k1::rand::RngCore as _;
use bitcoin::secp256k1::PublicKey;
use bitcoin::{FeeRate, ScriptBuf, Transaction};
use lightning::chain::chaininterface::{ConfirmationTarget, FeeEstimator as _};
use lightning::chain::channelmonitor::Balance;
use lightning::ln::channel_state::{ChannelDetails, ChannelShutdownState};
use lightning::log_error;
use lightning::log_info;
use lightning::util::logger::Logger as _;
use lightning::util::persist::KVStoreSync;
use lightning_persister::fs_store::FilesystemStore;

use crate::chain::{Broadcaster, ChainSource};
use crate::fees::CachedFeeEstimator;
use crate::onchain_send::{format_btc, OnchainSendError, TxSpec};
use crate::types::{ChainMonitor, ChannelManager, Logger, PeerManager};
use crate::util::{hex_str, peer_is_connected};
use crate::vss::known_peers::{KnownPeer, KnownPeersStore};
use crate::wallet::OnchainWallet;

/// Minimum channel size (PWA `MIN_CHANNEL_SATS`, `OpenChannel.tsx:29`).
pub const MIN_CHANNEL_SATS: u64 = 20_000;

/// Maximum channel size — the non-wumbo protocol limit (PWA
/// `MAX_CHANNEL_SATS`, `OpenChannel.tsx:31`).
pub const MAX_CHANNEL_SATS: u64 = 16_777_215;

/// Approximate funding tx vsize for the open-fee estimate (PWA
/// `APPROX_FUNDING_TX_VBYTES`, `OpenChannel.tsx:34`).
pub const APPROX_FUNDING_TX_VBYTES: u64 = 140;

/// Mutual-close tx weight for the pre-close estimate (PWA
/// `COOP_CLOSE_WEIGHT_WU`, `estimate.ts:16`).
pub(crate) const COOP_CLOSE_WEIGHT_WU: u64 = 700;

/// One force-close output swept to P2WPKH (PWA `SWEEP_VBYTES`, `estimate.ts:19`).
pub(crate) const SWEEP_VBYTES: u64 = 140;

/// Anchor spend + CPFP headroom (PWA `CPFP_VBYTES`, `estimate.ts:21`).
pub(crate) const CPFP_VBYTES: u64 = 200;

/// Outbound peer dial + handshake budget (PWA `CONNECTION_TIMEOUT_MS`,
/// `peer-connection.ts:12`).
pub(crate) const PEER_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// The reason string LDK sends the counterparty on a user force close (PWA
/// `context.tsx:803`).
pub(crate) const FORCE_CLOSE_REASON: &str = "User-initiated force close";

/// Typed `pubkey@host:port` parse failures, mirroring the PWA's distinct
/// messages (`peer-connection.ts:149-175`). Hostnames are additionally a
/// typed error here: the core dials `SocketAddr`s (U12 decision — the
/// known-peers reconnect loop skips non-IP hosts the same way).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerAddressError {
    /// No `@` separator.
    MissingAt,
    /// No `:` after the `@` (no port).
    MissingPort,
    /// The port is not a number in 1..=65535.
    InvalidPort,
    /// The pubkey is not 66 lowercase hex chars encoding a valid point.
    InvalidPubkey,
    /// The host is a DNS name; only ip:port is dialable (typed, not silent).
    HostnameUnsupported { host: String },
    /// The host is neither an IP address nor a plausible hostname.
    InvalidHost { host: String },
}

impl std::fmt::Display for PeerAddressError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PeerAddressError::MissingAt => {
                write!(f, "Invalid peer address: expected pubkey@host:port")
            }
            PeerAddressError::MissingPort => {
                write!(f, "Invalid peer address: expected host:port after @")
            }
            PeerAddressError::InvalidPort => write!(
                f,
                "Invalid peer address: port must be a number between 1 and 65535"
            ),
            PeerAddressError::InvalidPubkey => write!(
                f,
                "Invalid peer address: pubkey must be 66 lowercase hex characters"
            ),
            PeerAddressError::HostnameUnsupported { host } => write!(
                f,
                "Invalid peer address: hostname {host} is not supported; use an ip:port address"
            ),
            PeerAddressError::InvalidHost { host } => {
                write!(f, "Invalid peer address: {host} is not a valid host")
            }
        }
    }
}

impl std::error::Error for PeerAddressError {}

/// Typed channel/peer operation failures (U9). Each variant renders a
/// distinct message; the PWA-visible ones carry the PWA's exact copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChannelsError {
    /// The node is not running.
    NotRunning,
    /// The peer address failed to parse.
    InvalidAddress(PeerAddressError),
    /// A bare pubkey argument failed to parse.
    InvalidPubkey,
    /// Dial or handshake failed/timed out.
    ConnectFailed { detail: String },
    /// The known-peers store could not be written.
    PersistFailed { detail: String },
    /// Forget refused: channels with this peer are open (PWA copy).
    PeerHasOpenChannels,
    /// Open amount below [`MIN_CHANNEL_SATS`] (PWA copy).
    AmountBelowMinimum,
    /// Open amount above [`MAX_CHANNEL_SATS`] (PWA copy).
    AmountAboveMaximum,
    /// Amount plus the estimated funding fee exceeds the spendable balance
    /// (PWA copy).
    AmountExceedsBalance,
    /// `create_channel` failed.
    OpenFailed { detail: String },
    /// No open channel has this id.
    ChannelNotFound,
    /// The coop or force close call failed.
    CloseFailed { detail: String },
}

impl std::fmt::Display for ChannelsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChannelsError::NotRunning => write!(f, "the node is not running"),
            ChannelsError::InvalidAddress(e) => write!(f, "{e}"),
            ChannelsError::InvalidPubkey => {
                write!(f, "Invalid pubkey: must be 66 lowercase hex characters")
            }
            ChannelsError::ConnectFailed { detail } => {
                write!(f, "Failed to connect to peer: {detail}")
            }
            ChannelsError::PersistFailed { detail } => {
                write!(f, "Failed to persist known peer: {detail}")
            }
            // The PWA's exact copy (`context.tsx:866`).
            ChannelsError::PeerHasOpenChannels => {
                write!(f, "Cannot forget peer with open channels")
            }
            // The PWA's exact copy (`OpenChannel.tsx:88,93,101`).
            ChannelsError::AmountBelowMinimum => write!(
                f,
                "Minimum channel size is {}",
                format_btc(MIN_CHANNEL_SATS)
            ),
            ChannelsError::AmountAboveMaximum => write!(
                f,
                "Maximum channel size is {}",
                format_btc(MAX_CHANNEL_SATS)
            ),
            ChannelsError::AmountExceedsBalance => {
                write!(f, "Amount plus fees exceeds available balance")
            }
            ChannelsError::OpenFailed { detail } => {
                write!(f, "Failed to initiate channel opening: {detail}")
            }
            ChannelsError::ChannelNotFound => write!(f, "Channel not found"),
            ChannelsError::CloseFailed { detail } => write!(f, "Close failed: {detail}"),
        }
    }
}

impl std::error::Error for ChannelsError {}

impl From<PeerAddressError> for ChannelsError {
    fn from(error: PeerAddressError) -> Self {
        ChannelsError::InvalidAddress(error)
    }
}

/// One row of the Peers screen (PWA `PeerEntry`, `Peers.tsx:16-23`): the
/// union of saved (known) peers and currently connected peers.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct PeerView {
    /// 66-char hex node id.
    pub pubkey: String,
    /// `host:port` when the peer is saved; `None` for connected-only peers.
    pub address: Option<String>,
    /// Whether a handshake-complete connection exists right now.
    pub connected: bool,
    /// Whether the peer is in the saved known-peers store (Forget shows only
    /// for known peers).
    pub known: bool,
    /// Open channels with this peer (Forget is disabled unless 0).
    pub channel_count: u32,
}

/// The Peers screen's state label (PWA `Peers.tsx:263-269`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum ChannelStateLabel {
    /// Usable now.
    Active,
    /// Ready but not currently usable (peer offline).
    Ready,
    /// Awaiting funding confirmation.
    Pending,
    /// A shutdown is in progress.
    Closing,
}

/// One channel row (PWA `ChannelInfo`, `Peers.tsx:58-70`).
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ChannelView {
    /// 64-char hex channel id.
    pub channel_id: String,
    /// The counterparty's 66-char hex node id.
    pub counterparty_pubkey: String,
    /// Active / Ready / Pending / Closing.
    pub state: ChannelStateLabel,
    /// Total channel capacity in sats.
    pub capacity_sats: u64,
    /// Outbound (send) capacity in msat.
    pub outbound_msat: u64,
    /// Inbound (receive) capacity in msat.
    pub inbound_msat: u64,
    /// Our unspendable punishment reserve, when known.
    pub reserve_sats: Option<u64>,
    /// Whether the channel is usable right now.
    pub usable: bool,
    /// In-flight HTLCs (inbound + outbound) — the CloseChannel screen's
    /// "N in-flight payments" warning (PWA `CloseChannel.tsx:406-413`).
    pub pending_htlc_count: u32,
}

/// The open-channel review numbers (PWA `OpenChannel.tsx:97-98`): the 6-block
/// rate and `rate × 140 vB`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct OpenFeeEstimate {
    /// 6-block sat/vB, ceil'd (PWA `getFeeRate(6)` + `Math.ceil`).
    pub fee_rate_sat_per_vb: u64,
    /// `fee_rate × 140 vB`.
    pub estimated_fee_sats: u64,
}

/// Who pays the close-transaction fee (PWA `CloseEstimate.feePayer`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum CloseFeePayer {
    /// We funded the channel.
    You,
    /// The counterparty funded the channel.
    Counterparty,
    /// Could not be determined.
    Unknown,
}

/// Pre-close estimate (PWA `CloseEstimate`, `estimate.ts:36-57`): every field
/// independently nullable — `None` renders a placeholder. Informational only;
/// producing it NEVER gates or fails a close.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct CloseEstimate {
    pub fee_payer: CloseFeePayer,
    /// Estimated mutual-close tx fee (paid by the funder).
    pub coop_close_fee_sats: Option<u64>,
    /// Force close: commitment tx fee, pre-committed in the channel (0 when
    /// the counterparty funds).
    pub commitment_fee_sats: Option<u64>,
    /// Force close, anchor channels: estimated CPFP cost.
    pub cpfp_fee_sats: Option<u64>,
    /// Force close: estimated sweep-back-to-wallet cost.
    pub sweep_fee_sats: Option<u64>,
    /// What the user pays for a cooperative close.
    pub coop_total_you_pay_sats: Option<u64>,
    /// What the user pays for a force close.
    pub force_total_you_pay_sats: Option<u64>,
    /// Claimable balance if the channel closed now (excludes in-flight HTLCs).
    pub expected_back_sats: Option<u64>,
    /// Force close: blocks until funds can be swept (to_self_delay).
    pub timelock_blocks: Option<u16>,
    pub pending_htlc_count: Option<u32>,
    pub is_anchor: Option<bool>,
}

impl CloseEstimate {
    /// The all-`None` estimate: "estimate unavailable" for every field.
    pub(crate) fn unavailable() -> Self {
        Self {
            fee_payer: CloseFeePayer::Unknown,
            coop_close_fee_sats: None,
            commitment_fee_sats: None,
            cpfp_fee_sats: None,
            sweep_fee_sats: None,
            coop_total_you_pay_sats: None,
            force_total_you_pay_sats: None,
            expected_back_sats: None,
            timelock_blocks: None,
            pending_htlc_count: None,
            is_anchor: None,
        }
    }
}

/// Parses `pubkey@host:port` into a dialable target. Validation order and
/// messages mirror the PWA's `parsePeerAddress` (`peer-connection.ts:149-175`)
/// — split at the first `@`, port from the LAST `:`, port checked before the
/// pubkey — with the hostname case a typed error (the core dials
/// `SocketAddr`s only; `LspConfig` is `SocketAddr`-typed the same way).
pub fn parse_peer_address(address: &str) -> Result<(PublicKey, SocketAddr), PeerAddressError> {
    let address = address.trim();
    let (pubkey_str, host_port) = address.split_once('@').ok_or(PeerAddressError::MissingAt)?;
    // Port from the LAST ':' (the PWA's lastIndexOf), checked before the
    // pubkey to match the PWA's error order.
    let (host, port_str) = host_port
        .rsplit_once(':')
        .ok_or(PeerAddressError::MissingPort)?;
    let _port: u16 = port_str
        .parse()
        .ok()
        .filter(|port| *port >= 1)
        .ok_or(PeerAddressError::InvalidPort)?;
    // 66 LOWERCASE hex chars (the PWA's regex) encoding a valid point.
    if pubkey_str.len() != 66
        || !pubkey_str
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(PeerAddressError::InvalidPubkey);
    }
    let pubkey = PublicKey::from_str(pubkey_str).map_err(|_| PeerAddressError::InvalidPubkey)?;
    // The whole host:port as a SocketAddr covers v4 and bracketed v6 (port 0
    // was already rejected above).
    if let Ok(socket_addr) = host_port.parse::<SocketAddr>() {
        return Ok((pubkey, socket_addr));
    }
    // Not an IP: distinguish a legitimate hostname (typed unsupported) from
    // garbage, using the PWA's host character class.
    if !host.is_empty()
        && host
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
    {
        return Err(PeerAddressError::HostnameUnsupported {
            host: host.to_string(),
        });
    }
    Err(PeerAddressError::InvalidHost {
        host: host.to_string(),
    })
}

/// Open-channel bounds (PWA `OpenChannel.tsx:87-94`).
pub(crate) fn check_open_amount(amount_sats: u64) -> Result<(), ChannelsError> {
    if amount_sats < MIN_CHANNEL_SATS {
        return Err(ChannelsError::AmountBelowMinimum);
    }
    if amount_sats > MAX_CHANNEL_SATS {
        return Err(ChannelsError::AmountAboveMaximum);
    }
    Ok(())
}

/// The open-fee estimate at `fee_rate_sat_per_vb`: `rate × 140 vB` (PWA
/// `OpenChannel.tsx:97-98`).
pub(crate) fn open_fee_estimate(fee_rate_sat_per_vb: u64) -> OpenFeeEstimate {
    OpenFeeEstimate {
        fee_rate_sat_per_vb,
        estimated_fee_sats: fee_rate_sat_per_vb * APPROX_FUNDING_TX_VBYTES,
    }
}

/// A random `user_channel_id` from 8 random bytes — 64 bits of entropy in the
/// low half of the u128, matching the PWA's layout (`context.tsx:759-764`:
/// 8 bytes accumulated big-endian, never the full 16).
pub(crate) fn random_user_channel_id() -> u128 {
    let mut bytes = [0u8; 8];
    OsRng.fill_bytes(&mut bytes);
    u128::from(u64::from_be_bytes(bytes))
}

// ---------------------------------------------------------------------------
// Funding-flow persistence (PWA `ldk_funding_txs` + `ldk_channel_id_map`)
// ---------------------------------------------------------------------------

pub(crate) const FUNDING_TX_PRIMARY_NAMESPACE: &str = "funding_txs";
pub(crate) const FUNDING_TX_SECONDARY_NAMESPACE: &str = "";
pub(crate) const CHANNEL_ID_MAP_PRIMARY_NAMESPACE: &str = "channel_id_map";
pub(crate) const CHANNEL_ID_MAP_SECONDARY_NAMESPACE: &str = "";

/// Local KVStore persistence for the funding flow (U9): funding tx bytes
/// keyed by TEMPORARY channel id (the PWA's `ldk_funding_txs` IDB store) and
/// the real→temporary channel-id map recorded at `ChannelPending` (the PWA's
/// `ldk_channel_id_map`). Local-only, like the PWA — never on VSS.
pub(crate) struct FundingStore {
    kv_store: Arc<FilesystemStore>,
    logger: Arc<Logger>,
}

impl FundingStore {
    pub(crate) fn new(kv_store: Arc<FilesystemStore>, logger: Arc<Logger>) -> Self {
        Self { kv_store, logger }
    }

    /// Persists the signed funding tx under the temporary channel id. MUST
    /// succeed before LDK is notified (write-order invariant).
    pub(crate) fn persist_funding_tx(
        &self,
        temp_channel_id_hex: &str,
        tx: &Transaction,
    ) -> Result<(), lightning::io::Error> {
        self.kv_store.write(
            FUNDING_TX_PRIMARY_NAMESPACE,
            FUNDING_TX_SECONDARY_NAMESPACE,
            temp_channel_id_hex,
            bitcoin::consensus::encode::serialize(tx),
        )
    }

    /// The persisted funding tx for a temporary channel id, if any.
    pub(crate) fn funding_tx(&self, temp_channel_id_hex: &str) -> Option<Transaction> {
        let bytes = self
            .kv_store
            .read(
                FUNDING_TX_PRIMARY_NAMESPACE,
                FUNDING_TX_SECONDARY_NAMESPACE,
                temp_channel_id_hex,
            )
            .ok()?;
        bitcoin::consensus::encode::deserialize(&bytes).ok()
    }

    /// Drops a persisted funding tx (after broadcast or on DiscardFunding).
    pub(crate) fn remove_funding_tx(&self, temp_channel_id_hex: &str) {
        let _ = self.kv_store.remove(
            FUNDING_TX_PRIMARY_NAMESPACE,
            FUNDING_TX_SECONDARY_NAMESPACE,
            temp_channel_id_hex,
            false,
        );
    }

    /// Records real→temporary at `ChannelPending`, so `DiscardFunding` (which
    /// carries the REAL id) can find the funding tx (keyed by the temp id).
    pub(crate) fn record_channel_id_map(&self, channel_id_hex: &str, temp_channel_id_hex: &str) {
        if let Err(e) = self.kv_store.write(
            CHANNEL_ID_MAP_PRIMARY_NAMESPACE,
            CHANNEL_ID_MAP_SECONDARY_NAMESPACE,
            channel_id_hex,
            temp_channel_id_hex.as_bytes().to_vec(),
        ) {
            log_error!(
                self.logger,
                "Failed to persist the channel id mapping for {channel_id_hex}: {e}"
            );
        }
    }

    /// The temporary id recorded for a real channel id, if any.
    pub(crate) fn temp_id_for(&self, channel_id_hex: &str) -> Option<String> {
        let bytes = self
            .kv_store
            .read(
                CHANNEL_ID_MAP_PRIMARY_NAMESPACE,
                CHANNEL_ID_MAP_SECONDARY_NAMESPACE,
                channel_id_hex,
            )
            .ok()?;
        String::from_utf8(bytes).ok()
    }

    /// Drops the map entry for a real channel id.
    pub(crate) fn remove_channel_id_map(&self, channel_id_hex: &str) {
        let _ = self.kv_store.remove(
            CHANNEL_ID_MAP_PRIMARY_NAMESPACE,
            CHANNEL_ID_MAP_SECONDARY_NAMESPACE,
            channel_id_hex,
            false,
        );
    }
}

/// Typed funding-flow failures, for the event handler's logging and the
/// write-order tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FundingFlowError {
    /// The funding tx could not be built/signed from the on-chain wallet.
    Build { detail: String },
    /// The funding tx could not be persisted; LDK was NOT notified and the
    /// channel will time out — no fund loss, nothing was broadcast.
    Persist { detail: String },
    /// `funding_transaction_generated` was rejected by LDK.
    Notify { detail: String },
}

impl std::fmt::Display for FundingFlowError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FundingFlowError::Build { detail } => {
                write!(f, "failed to build the funding tx: {detail}")
            }
            FundingFlowError::Persist { detail } => {
                write!(f, "failed to persist the funding tx: {detail}")
            }
            FundingFlowError::Notify { detail } => {
                write!(f, "funding_transaction_generated failed: {detail}")
            }
        }
    }
}

/// `FundingGenerationReady` (PWA `event-handler.ts:569-648`): build the
/// funding tx from the on-chain wallet at the 6-block rate, persist it keyed
/// by the temporary channel id BEFORE `notify` (which calls LDK's
/// `funding_transaction_generated_manual_broadcast`), and never broadcast
/// here — the broadcast waits for `FundingTxBroadcastSafe`.
///
/// A persist failure aborts WITHOUT notifying LDK (the channel times out; no
/// fund loss since nothing was broadcast) — the PWA's exact behavior.
pub(crate) fn handle_funding_generation_ready(
    funding: &FundingStore,
    wallet: &OnchainWallet,
    fee_rate_sat_per_vb: u64,
    temp_channel_id_hex: &str,
    channel_value_satoshis: u64,
    output_script: ScriptBuf,
    notify: impl FnOnce(Transaction) -> Result<(), String>,
) -> Result<(), FundingFlowError> {
    let fee_rate =
        FeeRate::from_sat_per_vb(fee_rate_sat_per_vb).ok_or_else(|| FundingFlowError::Build {
            detail: format!("fee rate {fee_rate_sat_per_vb} sat/vB overflows"),
        })?;
    // U8's build seam: untrusted-pending exclusion, sign, and the changeset
    // persisted BEFORE the tx leaves the wallet (the PWA's putChangeset after
    // funding, done pre-notify here — strictly safer).
    let tx = wallet
        .create_onchain_tx(
            &TxSpec::FundingOutput {
                script: output_script,
                amount_sats: channel_value_satoshis,
            },
            fee_rate,
            |_| Ok(()),
            |failure| OnchainSendError::BuildFailed {
                detail: format!("{failure:?}"),
            },
        )
        .map_err(|e| FundingFlowError::Build {
            detail: e.to_string(),
        })?;

    // THE write-order invariant: persist BEFORE notifying LDK. A failed
    // persist aborts the channel (it times out) — no fund loss, nothing was
    // broadcast (PWA `event-handler.ts:586-600`).
    funding
        .persist_funding_tx(temp_channel_id_hex, &tx)
        .map_err(|e| FundingFlowError::Persist {
            detail: e.to_string(),
        })?;

    notify(tx).map_err(|detail| FundingFlowError::Notify { detail })
}

/// The outcome of a `FundingTxBroadcastSafe`, for tests and logging.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BroadcastSafeOutcome {
    /// The persisted tx went to the broadcaster; the entry was dropped.
    Broadcast { txid: String },
    /// No persisted tx for this temporary channel id (PWA logs and skips).
    MissingTx,
}

/// `FundingTxBroadcastSafe` (PWA `event-handler.ts:651-656,849-861`): read
/// the persisted funding tx by TEMPORARY channel id, hand it to the
/// persist-first broadcaster, and drop the entry.
pub(crate) fn handle_funding_tx_broadcast_safe(
    funding: &FundingStore,
    broadcaster: &Broadcaster,
    temp_channel_id_hex: &str,
) -> BroadcastSafeOutcome {
    use lightning::chain::chaininterface::BroadcasterInterface as _;
    match funding.funding_tx(temp_channel_id_hex) {
        Some(tx) => {
            broadcaster.broadcast_transactions(&[&tx]);
            funding.remove_funding_tx(temp_channel_id_hex);
            BroadcastSafeOutcome::Broadcast {
                txid: tx.compute_txid().to_string(),
            }
        }
        None => BroadcastSafeOutcome::MissingTx,
    }
}

/// `DiscardFunding` (PWA `event-handler.ts:770-790`): look up the temporary
/// id from the map recorded at `ChannelPending`, then delete the orphaned
/// funding tx and the mapping. Returns whether anything was cleaned.
pub(crate) fn handle_discard_funding(funding: &FundingStore, channel_id_hex: &str) -> bool {
    match funding.temp_id_for(channel_id_hex) {
        Some(temp_id_hex) => {
            funding.remove_funding_tx(&temp_id_hex);
            funding.remove_channel_id_map(channel_id_hex);
            true
        }
        None => false,
    }
}

// ---------------------------------------------------------------------------
// Peers
// ---------------------------------------------------------------------------

/// Forget guard (PWA `context.tsx:852-868`): refuse to forget a peer while
/// any open channel's counterparty matches.
pub(crate) fn ensure_no_open_channels_with(
    open_counterparty_pubkeys: impl IntoIterator<Item = String>,
    pubkey_hex: &str,
) -> Result<(), ChannelsError> {
    if open_counterparty_pubkeys
        .into_iter()
        .any(|counterparty| counterparty == pubkey_hex)
    {
        return Err(ChannelsError::PeerHasOpenChannels);
    }
    Ok(())
}

/// Auto-forget on `ChannelClosed` (PWA `context.tsx:1233-1244`): when the
/// LAST channel with a peer closes, drop it from known peers so the
/// reconnect loop stops dialing it. Best-effort: a store failure is logged,
/// never replayed (peers are convenience state).
pub(crate) fn auto_forget_on_channel_closed(
    known_peers: &KnownPeersStore,
    counterparty_pubkey_hex: &str,
    still_has_channels: bool,
    logger: &Arc<Logger>,
) {
    if still_has_channels {
        return;
    }
    if let Err(e) = known_peers.remove(counterparty_pubkey_hex) {
        log_error!(
            logger,
            "Failed to remove known peer {counterparty_pubkey_hex} after channel close: {e}"
        );
    }
}

/// Merges saved and connected peers into the Peers screen's rows (PWA
/// `Peers.tsx:79-99`): union of pubkeys, connected first, then by pubkey.
pub(crate) fn build_peer_views(
    known: &BTreeMap<String, KnownPeer>,
    connected: &HashSet<String>,
    channel_counts: &HashMap<String, u32>,
) -> Vec<PeerView> {
    let mut pubkeys: Vec<String> = known.keys().cloned().collect();
    for pubkey in connected {
        if !known.contains_key(pubkey) {
            pubkeys.push(pubkey.clone());
        }
    }
    let mut views: Vec<PeerView> = pubkeys
        .into_iter()
        .map(|pubkey| {
            let peer = known.get(&pubkey);
            PeerView {
                connected: connected.contains(&pubkey),
                known: peer.is_some(),
                address: peer.map(|peer| format!("{}:{}", peer.host, peer.port)),
                channel_count: channel_counts.get(&pubkey).copied().unwrap_or(0),
                pubkey,
            }
        })
        .collect();
    views.sort_by(|a, b| {
        b.connected
            .cmp(&a.connected)
            .then_with(|| a.pubkey.cmp(&b.pubkey))
    });
    views
}

/// Maps LDK `ChannelDetails` to the Peers screen row (PWA `Peers.tsx:53-77`,
/// state label `Peers.tsx:263-269`).
pub(crate) fn channel_view(details: &ChannelDetails) -> ChannelView {
    let shutting_down = details
        .channel_shutdown_state
        .is_some_and(|state| state != ChannelShutdownState::NotShuttingDown);
    let state = if shutting_down {
        ChannelStateLabel::Closing
    } else if details.is_usable {
        ChannelStateLabel::Active
    } else if details.is_channel_ready {
        ChannelStateLabel::Ready
    } else {
        ChannelStateLabel::Pending
    };
    ChannelView {
        channel_id: hex_str(&details.channel_id.0),
        counterparty_pubkey: details.counterparty.node_id.to_string(),
        state,
        capacity_sats: details.channel_value_satoshis,
        outbound_msat: details.outbound_capacity_msat,
        inbound_msat: details.inbound_capacity_msat,
        reserve_sats: details.unspendable_punishment_reserve,
        usable: details.is_usable,
        pending_htlc_count: (details.pending_inbound_htlcs.len()
            + details.pending_outbound_htlcs.len()) as u32,
    }
}

/// Dials `node_id` at `address` and waits for the BOLT8 handshake
/// (`list_peers` only reports handshake-complete peers), with the PWA's 15 s
/// budget. Already-connected returns immediately.
pub(crate) async fn dial_peer(
    peer_manager: Arc<PeerManager>,
    node_id: PublicKey,
    address: SocketAddr,
) -> Result<(), ChannelsError> {
    if peer_is_connected(&peer_manager, node_id) {
        return Ok(());
    }
    match lightning_net_tokio::connect_outbound(Arc::clone(&peer_manager), node_id, address).await {
        Some(connection_closed) => {
            tokio::spawn(connection_closed);
        }
        None => {
            return Err(ChannelsError::ConnectFailed {
                detail: format!("could not open a TCP connection to {address}"),
            })
        }
    }
    let started = std::time::Instant::now();
    while started.elapsed() < PEER_CONNECT_TIMEOUT {
        if peer_is_connected(&peer_manager, node_id) {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Err(ChannelsError::ConnectFailed {
        detail: "Connection timed out".to_string(),
    })
}

// ---------------------------------------------------------------------------
// Close estimate
// ---------------------------------------------------------------------------

/// The unambiguous claimable-on-close read (PWA `readOnCloseBalance`,
/// `estimate.ts:86-102`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OnCloseBalance {
    pub(crate) commitment_fee_sats: u64,
    pub(crate) amount_sats: u64,
}

/// Everything [`compute_close_estimate`] consumes, each field independently
/// optional so any failed read degrades that field alone (PWA parity).
#[derive(Debug, Clone, Default)]
pub(crate) struct CloseEstimateInputs {
    pub(crate) is_outbound: Option<bool>,
    pub(crate) timelock_blocks: Option<u16>,
    pub(crate) pending_htlc_count: Option<u32>,
    pub(crate) is_anchor: Option<bool>,
    /// Set ONLY when exactly one `ClaimableOnChannelClose` remains after
    /// ignoring every other channel (the unambiguous read).
    pub(crate) on_close: Option<OnCloseBalance>,
    pub(crate) outbound_capacity_msat: Option<u64>,
    /// `ChannelCloseMinimum` in sat/kw.
    pub(crate) coop_close_sat_per_kw: Option<u32>,
    /// 6-block sat/vB (the sweep estimate's rate).
    pub(crate) sweep_rate_sat_per_vb: Option<u64>,
    /// 3-block sat/vB (the CPFP estimate's rate).
    pub(crate) urgent_rate_sat_per_vb: Option<u64>,
}

/// Pure close-estimate arithmetic (PWA `estimateClose`, `estimate.ts:108-219`):
/// nullable per field, never errors.
pub(crate) fn compute_close_estimate(inputs: &CloseEstimateInputs) -> CloseEstimate {
    let mut estimate = CloseEstimate::unavailable();

    estimate.fee_payer = match inputs.is_outbound {
        Some(true) => CloseFeePayer::You,
        Some(false) => CloseFeePayer::Counterparty,
        None => CloseFeePayer::Unknown,
    };
    estimate.timelock_blocks = inputs.timelock_blocks;
    estimate.pending_htlc_count = inputs.pending_htlc_count;
    estimate.is_anchor = inputs.is_anchor;

    // The unambiguous claimable read wins; otherwise degrade to the outbound
    // capacity (PWA `estimate.ts:164-180`).
    if let Some(on_close) = inputs.on_close {
        estimate.commitment_fee_sats = Some(on_close.commitment_fee_sats);
        estimate.expected_back_sats = Some(on_close.amount_sats);
    } else {
        estimate.expected_back_sats = inputs.outbound_capacity_msat.map(|msat| msat / 1_000);
    }

    // Fee legs (PWA `estimate.ts:182-201`): coop from ChannelCloseMinimum
    // sat/kw × 700 WU; sweep 140 vB at the 6-block rate; CPFP 200 vB at the
    // 3-block rate — but ONLY when anchor support is known (an unknown flag
    // must not make the force close look cheaper than it may be).
    estimate.coop_close_fee_sats = inputs
        .coop_close_sat_per_kw
        .map(|sat_per_kw| (u64::from(sat_per_kw) * COOP_CLOSE_WEIGHT_WU + 500) / 1_000);
    estimate.sweep_fee_sats = inputs
        .sweep_rate_sat_per_vb
        .map(|rate| (rate * SWEEP_VBYTES).max(1));
    estimate.cpfp_fee_sats = match inputs.is_anchor {
        None => None,
        Some(false) => Some(0),
        Some(true) => inputs
            .urgent_rate_sat_per_vb
            .map(|rate| (rate * CPFP_VBYTES).max(1)),
    };

    // Totals (PWA `estimate.ts:203-216`): coop = the coop fee iff we funded,
    // else 0; force = commitment (funder only) + CPFP + sweep, withheld when
    // an outbound channel's commitment fee is unknown.
    if let (Some(is_outbound), Some(coop_fee)) = (inputs.is_outbound, estimate.coop_close_fee_sats)
    {
        estimate.coop_total_you_pay_sats = Some(if is_outbound { coop_fee } else { 0 });
    }
    if let (Some(is_outbound), Some(sweep_fee), Some(cpfp_fee)) = (
        inputs.is_outbound,
        estimate.sweep_fee_sats,
        estimate.cpfp_fee_sats,
    ) {
        if estimate.commitment_fee_sats.is_some() || !is_outbound {
            let commitment = if is_outbound {
                estimate.commitment_fee_sats.unwrap_or(0)
            } else {
                0
            };
            estimate.force_total_you_pay_sats = Some(commitment + cpfp_fee + sweep_fee);
        }
    }

    estimate
}

/// Live-node close estimate: gathers [`CloseEstimateInputs`] from the channel
/// manager, chain monitor, and fee cache, then computes. Returns the
/// all-`None` estimate when the channel is unknown — NEVER an error (the
/// close screen must render regardless; PWA `estimate.ts:28-35`).
pub(crate) fn estimate_close(
    channel_manager: &ChannelManager,
    chain_monitor: &ChainMonitor,
    fee_estimator: &CachedFeeEstimator,
    channel_id_hex: &str,
) -> CloseEstimate {
    let all = channel_manager.list_channels();
    let Some(channel) = all
        .iter()
        .find(|details| hex_str(&details.channel_id.0) == channel_id_hex)
    else {
        return CloseEstimate::unavailable();
    };
    let others: Vec<&ChannelDetails> = all
        .iter()
        .filter(|details| details.channel_id != channel.channel_id)
        .collect();

    // The unambiguous claimable read (PWA `readOnCloseBalance`): ignore every
    // OTHER channel and trust the result only when exactly one
    // ClaimableOnChannelClose entry remains.
    let on_close_entries: Vec<Balance> = chain_monitor
        .get_claimable_balances(&others)
        .into_iter()
        .filter(|balance| matches!(balance, Balance::ClaimableOnChannelClose { .. }))
        .collect();
    let on_close = match on_close_entries.as_slice() {
        [Balance::ClaimableOnChannelClose {
            balance_candidates,
            confirmed_balance_candidate_index,
            ..
        }] => balance_candidates
            .get(*confirmed_balance_candidate_index)
            .map(|candidate| OnCloseBalance {
                commitment_fee_sats: candidate.transaction_fee_satoshis,
                amount_sats: candidate.amount_satoshis,
            }),
        _ => None,
    };

    let coop_sat_per_kw =
        fee_estimator.get_est_sat_per_1000_weight(ConfirmationTarget::ChannelCloseMinimum);
    // PWA getFeeRate(3): UrgentOnChainSweep targets 3 blocks (KTD-9);
    // sat/kw → sat/vB.
    let urgent_sat_per_vb = u64::from(
        fee_estimator.get_est_sat_per_1000_weight(ConfirmationTarget::UrgentOnChainSweep),
    )
    .div_ceil(250);

    compute_close_estimate(&CloseEstimateInputs {
        is_outbound: Some(channel.is_outbound),
        timelock_blocks: channel.force_close_spend_delay,
        pending_htlc_count: Some(
            (channel.pending_inbound_htlcs.len() + channel.pending_outbound_htlcs.len()) as u32,
        ),
        is_anchor: channel
            .channel_type
            .as_ref()
            .map(|features| features.supports_anchors_zero_fee_htlc_tx()),
        on_close,
        outbound_capacity_msat: Some(channel.outbound_capacity_msat),
        coop_close_sat_per_kw: Some(coop_sat_per_kw),
        sweep_rate_sat_per_vb: Some(fee_estimator.onchain_send_rate_sat_per_vb()),
        urgent_rate_sat_per_vb: Some(urgent_sat_per_vb),
    })
}

// ---------------------------------------------------------------------------
// Event-handler context
// ---------------------------------------------------------------------------

/// The handles `handle_ldk_event` needs for the U9 channel events, cloned out
/// of `NodeComponents` once when the background processor spawns.
pub(crate) struct ChannelEventContext {
    pub(crate) channel_manager: Arc<ChannelManager>,
    pub(crate) onchain_wallet: Arc<OnchainWallet>,
    pub(crate) broadcaster: Arc<Broadcaster>,
    pub(crate) chain_source: Arc<ChainSource>,
    pub(crate) known_peers: Arc<KnownPeersStore>,
    pub(crate) funding: Arc<FundingStore>,
}

/// Builds the funding tx and drives the persist-then-notify order for a live
/// `FundingGenerationReady` (the node's event switchboard calls this;
/// failures are logged — LDK never replays this event, and an un-notified
/// channel times out fund-safely).
pub(crate) fn on_funding_generation_ready(
    ctx: &ChannelEventContext,
    temporary_channel_id: lightning::ln::types::ChannelId,
    counterparty_node_id: PublicKey,
    channel_value_satoshis: u64,
    output_script: ScriptBuf,
    logger: &Arc<Logger>,
) {
    let temp_hex = hex_str(&temporary_channel_id.0);
    let channel_manager = Arc::clone(&ctx.channel_manager);
    let result = handle_funding_generation_ready(
        &ctx.funding,
        &ctx.onchain_wallet,
        ctx.chain_source.onchain_send_fee_rate_sat_per_vb(),
        &temp_hex,
        channel_value_satoshis,
        output_script,
        |tx| {
            channel_manager
                .funding_transaction_generated_manual_broadcast(
                    temporary_channel_id,
                    counterparty_node_id,
                    tx,
                )
                .map_err(|e| format!("{e:?}"))
        },
    );
    match result {
        Ok(()) => log_info!(
            logger,
            "Funding tx registered for temporary channel {temp_hex} \
             ({channel_value_satoshis} sats)"
        ),
        Err(e) => log_error!(logger, "FundingGenerationReady for {temp_hex}: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    use bitcoin::Network;

    use crate::chain::PendingBroadcasts;
    use crate::vss::store::{RetryTuning, VssBackedStore, VssTransport};
    use crate::vss::test_support::MockTransport;

    const PUBKEY: &str = "034066e29e402d9cf55af1ae1026cc5adf92eed1e0e421785442f53717ad1453b0";
    const PUBKEY_B: &str = "02eec7245d6b7d2ccb30380bfbe2a3648cd7a942653f5aa340edcea1f283686619";

    // ------------------------------------------------------------------
    // parse_peer_address matrix (plan: valid, missing port, bad pubkey)
    // ------------------------------------------------------------------

    #[test]
    fn parse_accepts_a_valid_ip_v4_address_and_trims_whitespace() {
        let (pubkey, addr) = parse_peer_address(&format!("  {PUBKEY}@64.23.159.177:9735  "))
            .expect("valid address must parse");
        assert_eq!(pubkey.to_string(), PUBKEY);
        assert_eq!(addr.to_string(), "64.23.159.177:9735");
    }

    #[test]
    fn parse_accepts_a_bracketed_ip_v6_address() {
        let (_, addr) = parse_peer_address(&format!("{PUBKEY}@[::1]:9736")).unwrap();
        assert_eq!(addr.to_string(), "[::1]:9736");
    }

    #[test]
    fn parse_rejects_a_missing_at_separator() {
        assert_eq!(
            parse_peer_address("nopubkeyhere"),
            Err(PeerAddressError::MissingAt)
        );
    }

    #[test]
    fn parse_rejects_a_missing_port() {
        assert_eq!(
            parse_peer_address(&format!("{PUBKEY}@64.23.159.177")),
            Err(PeerAddressError::MissingPort)
        );
    }

    #[test]
    fn parse_rejects_bad_ports() {
        for bad in ["0", "65536", "abc", ""] {
            assert_eq!(
                parse_peer_address(&format!("{PUBKEY}@1.2.3.4:{bad}")),
                Err(PeerAddressError::InvalidPort),
                "port {bad:?} must be rejected"
            );
        }
    }

    #[test]
    fn parse_rejects_bad_pubkeys() {
        // Wrong length, uppercase hex (the PWA requires lowercase), non-hex,
        // and a 66-char hex string that is not a curve point.
        let not_a_point = format!("02{}", "00".repeat(32));
        for bad in [
            "02abcd",
            &PUBKEY.to_uppercase(),
            &"zz".repeat(33),
            not_a_point.as_str(),
        ] {
            assert_eq!(
                parse_peer_address(&format!("{bad}@1.2.3.4:9735")),
                Err(PeerAddressError::InvalidPubkey),
                "pubkey {bad:?} must be rejected"
            );
        }
    }

    #[test]
    fn parse_rejects_hostnames_with_a_typed_error() {
        // Hostnames are valid in the PWA (ws proxy resolves them) but the
        // core dials SocketAddrs: typed error, consistent with the U12
        // known-peers reconnect skip.
        assert_eq!(
            parse_peer_address(&format!("{PUBKEY}@node.example.com:9735")),
            Err(PeerAddressError::HostnameUnsupported {
                host: "node.example.com".to_string()
            })
        );
        // A host that is not even hostname-shaped is a distinct error.
        assert_eq!(
            parse_peer_address(&format!("{PUBKEY}@not a host:9735")),
            Err(PeerAddressError::InvalidHost {
                host: "not a host".to_string()
            })
        );
    }

    #[test]
    fn parse_checks_the_port_before_the_pubkey_like_the_pwa() {
        // Both invalid: the PWA reports the port first.
        assert_eq!(
            parse_peer_address("junk@1.2.3.4:notaport"),
            Err(PeerAddressError::InvalidPort)
        );
    }

    // ------------------------------------------------------------------
    // Open-channel bounds and fee estimate (plan)
    // ------------------------------------------------------------------

    #[test]
    fn open_amount_bounds_match_the_pwa() {
        assert_eq!(
            check_open_amount(MIN_CHANNEL_SATS - 1),
            Err(ChannelsError::AmountBelowMinimum)
        );
        assert_eq!(check_open_amount(MIN_CHANNEL_SATS), Ok(()));
        assert_eq!(check_open_amount(MAX_CHANNEL_SATS), Ok(()));
        assert_eq!(
            check_open_amount(MAX_CHANNEL_SATS + 1),
            Err(ChannelsError::AmountAboveMaximum)
        );
        // The error copy is the PWA's, ₿-formatted.
        assert_eq!(
            ChannelsError::AmountBelowMinimum.to_string(),
            "Minimum channel size is \u{20BF}20,000"
        );
        assert_eq!(
            ChannelsError::AmountAboveMaximum.to_string(),
            "Maximum channel size is \u{20BF}16,777,215"
        );
    }

    #[test]
    fn open_fee_estimate_is_six_block_rate_times_140_vb() {
        assert_eq!(
            open_fee_estimate(5),
            OpenFeeEstimate {
                fee_rate_sat_per_vb: 5,
                estimated_fee_sats: 700,
            }
        );
        assert_eq!(open_fee_estimate(2).estimated_fee_sats, 280);
    }

    #[test]
    fn user_channel_id_uses_only_the_low_64_bits_like_the_pwa() {
        // The PWA accumulates 8 random bytes (never 16): the high half of the
        // u128 must be zero.
        for _ in 0..16 {
            assert_eq!(random_user_channel_id() >> 64, 0);
        }
    }

    // ------------------------------------------------------------------
    // Funding flow: write order, broadcast-safe, discard (plan)
    // ------------------------------------------------------------------

    fn funded_wallet(dir: &Path, sats: u64) -> OnchainWallet {
        let keys = crate::keys::derive_wallet_keys(
            &crate::keys::parse_mnemonic(crate::keys::tests::TEST_MNEMONIC).unwrap(),
            Network::Bitcoin,
        );
        let wallet = OnchainWallet::new(
            &keys.descriptor_external,
            &keys.descriptor_internal,
            Network::Bitcoin,
            Arc::new(FilesystemStore::new(PathBuf::from(dir).join("wallet"))),
            Arc::new(Logger),
        )
        .unwrap();
        crate::wallet::test_support::fund_confirmed(&wallet, sats);
        wallet
    }

    fn funding_store(dir: &Path) -> FundingStore {
        FundingStore::new(
            Arc::new(FilesystemStore::new(PathBuf::from(dir).join("funding"))),
            Arc::new(Logger),
        )
    }

    fn funding_script() -> ScriptBuf {
        // A P2WSH-shaped output script, like a real funding output.
        use bitcoin::hashes::Hash as _;
        ScriptBuf::new_p2wsh(&bitcoin::WScriptHash::from_byte_array([0x42; 32]))
    }

    /// THE write-order assertion (plan): the funding tx must be durable in
    /// the store BEFORE LDK is notified — asserted from inside the notify
    /// seam itself.
    #[test]
    fn funding_tx_is_persisted_before_ldk_is_notified() {
        let dir = tempfile::tempdir().unwrap();
        let wallet = funded_wallet(dir.path(), 200_000);
        let store = funding_store(dir.path());
        let temp_hex = "aa".repeat(32);

        let mut notified_tx: Option<Transaction> = None;
        handle_funding_generation_ready(
            &store,
            &wallet,
            2,
            &temp_hex,
            50_000,
            funding_script(),
            |tx| {
                // The invariant: at notify time the tx is already persisted,
                // byte-identical to what LDK receives.
                let persisted = store
                    .funding_tx(&temp_hex)
                    .expect("funding tx must be persisted BEFORE the notify call");
                assert_eq!(persisted, tx);
                notified_tx = Some(tx);
                Ok(())
            },
        )
        .expect("funding flow must succeed");

        let tx = notified_tx.expect("LDK must be notified");
        // The tx pays the funding script the exact channel value and carries
        // a final locktime (LDK rejects anti-fee-sniping locktimes).
        assert!(tx
            .output
            .iter()
            .any(|out| out.script_pubkey == funding_script() && out.value.to_sat() == 50_000));
        assert_eq!(tx.lock_time.to_consensus_u32(), 0, "nlocktime(0) required");
        // The entry stays persisted until FundingTxBroadcastSafe consumes it.
        assert!(store.funding_tx(&temp_hex).is_some());
    }

    /// A persist failure aborts WITHOUT notifying LDK (PWA: "abort the
    /// channel — no fund loss since the tx was never broadcast").
    #[cfg(unix)]
    #[test]
    fn funding_persist_failure_never_notifies_ldk() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().unwrap();
        let wallet = funded_wallet(dir.path(), 200_000);
        let store = funding_store(dir.path());
        let temp_hex = "bb".repeat(32);

        // Seed the namespace dir, then make it read-only.
        store
            .persist_funding_tx(
                "seed",
                &Transaction {
                    version: bitcoin::transaction::Version::TWO,
                    lock_time: bitcoin::absolute::LockTime::ZERO,
                    input: vec![],
                    output: vec![],
                },
            )
            .unwrap();
        let namespace_dir = dir
            .path()
            .join("funding")
            .join(FUNDING_TX_PRIMARY_NAMESPACE);
        let writable = std::fs::metadata(&namespace_dir).unwrap().permissions();
        std::fs::set_permissions(&namespace_dir, std::fs::Permissions::from_mode(0o555)).unwrap();

        let result = handle_funding_generation_ready(
            &store,
            &wallet,
            2,
            &temp_hex,
            50_000,
            funding_script(),
            |_| panic!("LDK must NOT be notified when the persist failed"),
        );
        std::fs::set_permissions(&namespace_dir, writable).unwrap();

        assert!(
            matches!(result, Err(FundingFlowError::Persist { .. })),
            "{result:?}"
        );
        assert!(store.funding_tx(&temp_hex).is_none());
    }

    /// A build failure (empty wallet) persists nothing and never notifies.
    #[test]
    fn funding_build_failure_persists_nothing_and_never_notifies() {
        let dir = tempfile::tempdir().unwrap();
        let wallet = funded_wallet(dir.path(), 1_000); // far below the channel value
        let store = funding_store(dir.path());
        let temp_hex = "cc".repeat(32);

        let result = handle_funding_generation_ready(
            &store,
            &wallet,
            2,
            &temp_hex,
            50_000,
            funding_script(),
            |_| panic!("LDK must NOT be notified when the build failed"),
        );
        assert!(
            matches!(result, Err(FundingFlowError::Build { .. })),
            "{result:?}"
        );
        assert!(store.funding_tx(&temp_hex).is_none());
    }

    #[test]
    fn broadcast_safe_broadcasts_the_persisted_tx_and_drops_the_entry() {
        let dir = tempfile::tempdir().unwrap();
        let store = funding_store(dir.path());
        let pending = Arc::new(PendingBroadcasts::new(
            Arc::new(FilesystemStore::new(dir.path().join("pending"))),
            Arc::new(Logger),
        ));
        let broadcaster = Broadcaster::new(Arc::clone(&pending), Arc::new(Logger));
        let temp_hex = "dd".repeat(32);

        let tx = Transaction {
            version: bitcoin::transaction::Version::TWO,
            lock_time: bitcoin::absolute::LockTime::ZERO,
            input: vec![],
            output: vec![bitcoin::TxOut {
                value: bitcoin::Amount::from_sat(50_000),
                script_pubkey: funding_script(),
            }],
        };
        store.persist_funding_tx(&temp_hex, &tx).unwrap();

        let outcome = handle_funding_tx_broadcast_safe(&store, &broadcaster, &temp_hex);
        assert_eq!(
            outcome,
            BroadcastSafeOutcome::Broadcast {
                txid: tx.compute_txid().to_string()
            }
        );
        // Persist-first broadcasting: the pending-broadcast store holds it.
        assert_eq!(pending.pending_txids(), vec![tx.compute_txid().to_string()]);
        // Consumed: the funding entry is gone, a replayed event is a no-op.
        assert!(store.funding_tx(&temp_hex).is_none());
        assert_eq!(
            handle_funding_tx_broadcast_safe(&store, &broadcaster, &temp_hex),
            BroadcastSafeOutcome::MissingTx
        );
    }

    #[test]
    fn discard_funding_cleans_up_via_the_channel_id_map() {
        let dir = tempfile::tempdir().unwrap();
        let store = funding_store(dir.path());
        let temp_hex = "ee".repeat(32);
        let real_hex = "ff".repeat(32);

        let tx = Transaction {
            version: bitcoin::transaction::Version::TWO,
            lock_time: bitcoin::absolute::LockTime::ZERO,
            input: vec![],
            output: vec![],
        };
        store.persist_funding_tx(&temp_hex, &tx).unwrap();
        // ChannelPending records real → temp.
        store.record_channel_id_map(&real_hex, &temp_hex);
        assert_eq!(store.temp_id_for(&real_hex).as_deref(), Some(&*temp_hex));

        // DiscardFunding carries the REAL id; both entries must go.
        assert!(handle_discard_funding(&store, &real_hex));
        assert!(store.funding_tx(&temp_hex).is_none());
        assert!(store.temp_id_for(&real_hex).is_none());
        // Unknown id: nothing to clean (PWA logs and moves on).
        assert!(!handle_discard_funding(&store, &real_hex));
    }

    // ------------------------------------------------------------------
    // Forget guard + auto-forget (plan)
    // ------------------------------------------------------------------

    #[test]
    fn forget_is_refused_while_a_channel_with_the_peer_is_open() {
        let open = vec![PUBKEY.to_string()];
        assert_eq!(
            ensure_no_open_channels_with(open.clone(), PUBKEY),
            Err(ChannelsError::PeerHasOpenChannels)
        );
        assert_eq!(
            ChannelsError::PeerHasOpenChannels.to_string(),
            "Cannot forget peer with open channels",
            "the PWA's exact copy"
        );
        assert_eq!(ensure_no_open_channels_with(open, PUBKEY_B), Ok(()));
        assert_eq!(ensure_no_open_channels_with(Vec::new(), PUBKEY), Ok(()));
    }

    fn known_peers_store(dir: &Path, rt: &tokio::runtime::Runtime) -> KnownPeersStore {
        let local = Arc::new(FilesystemStore::new(dir.join("store")));
        struct NullSink;
        impl crate::node::EventSink for NullSink {
            fn emit(&self, _event: crate::node::CoreEvent) {}
        }
        let vss = Arc::new(VssBackedStore::new(
            Some(Arc::new(MockTransport::new()) as Arc<dyn VssTransport>),
            Arc::clone(&local),
            rt.handle().clone(),
            dir,
            Arc::new(NullSink),
            Arc::new(Logger),
            RetryTuning {
                initial_backoff: Duration::from_millis(2),
                max_backoff: Duration::from_millis(10),
                degraded_after: Duration::from_millis(6),
                cm_attempt_timeout: Duration::from_millis(200),
            },
            HashMap::new(),
            std::collections::BTreeSet::new(),
            false,
        ));
        KnownPeersStore::load(local, vss, Arc::new(Logger))
    }

    #[test]
    fn auto_forget_removes_the_peer_only_when_its_last_channel_closed() {
        let dir = tempfile::tempdir().unwrap();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let store = known_peers_store(dir.path(), &rt);
        let logger = Arc::new(Logger);
        store.upsert(PUBKEY, "64.23.159.177", 9735).unwrap();

        // Another channel with the peer remains: keep it.
        auto_forget_on_channel_closed(&store, PUBKEY, true, &logger);
        assert!(store.all().contains_key(PUBKEY));

        // The LAST channel closed: forget (PWA context.tsx:1233-1244).
        auto_forget_on_channel_closed(&store, PUBKEY, false, &logger);
        assert!(!store.all().contains_key(PUBKEY));

        // Idempotent under event replay.
        auto_forget_on_channel_closed(&store, PUBKEY, false, &logger);
        assert!(store.all().is_empty());
    }

    // ------------------------------------------------------------------
    // Peer list merge (PWA Peers.tsx:79-99)
    // ------------------------------------------------------------------

    #[test]
    fn peer_views_union_saved_and_connected_sorted_connected_first() {
        let mut known = BTreeMap::new();
        known.insert(
            PUBKEY.to_string(),
            KnownPeer {
                host: "64.23.159.177".to_string(),
                port: 9735,
            },
        );
        let connected: HashSet<String> = [PUBKEY_B.to_string()].into();
        let counts: HashMap<String, u32> = [(PUBKEY.to_string(), 2)].into();

        let views = build_peer_views(&known, &connected, &counts);
        assert_eq!(views.len(), 2);
        // Connected-only peer sorts first.
        assert_eq!(views[0].pubkey, PUBKEY_B);
        assert!(views[0].connected);
        assert!(!views[0].known);
        assert_eq!(views[0].address, None);
        assert_eq!(views[0].channel_count, 0);
        // Saved-but-offline peer follows, with its address and channel count.
        assert_eq!(views[1].pubkey, PUBKEY);
        assert!(!views[1].connected);
        assert!(views[1].known);
        assert_eq!(views[1].address.as_deref(), Some("64.23.159.177:9735"));
        assert_eq!(views[1].channel_count, 2);
    }

    // ------------------------------------------------------------------
    // Close estimate (plan: nulls on ambiguity, never errors)
    // ------------------------------------------------------------------

    #[test]
    fn close_estimate_with_no_inputs_is_all_none_and_never_errors() {
        let estimate = compute_close_estimate(&CloseEstimateInputs::default());
        assert_eq!(estimate, CloseEstimate::unavailable());
    }

    #[test]
    fn close_estimate_full_inputs_matches_the_pwa_arithmetic() {
        let estimate = compute_close_estimate(&CloseEstimateInputs {
            is_outbound: Some(true),
            timelock_blocks: Some(144),
            pending_htlc_count: Some(2),
            is_anchor: Some(true),
            on_close: Some(OnCloseBalance {
                commitment_fee_sats: 1_100,
                amount_sats: 80_000,
            }),
            outbound_capacity_msat: Some(79_000_000),
            coop_close_sat_per_kw: Some(1_000), // × 700 WU / 1000 = 700
            sweep_rate_sat_per_vb: Some(5),     // × 140 = 700
            urgent_rate_sat_per_vb: Some(10),   // × 200 = 2,000
        });
        assert_eq!(estimate.fee_payer, CloseFeePayer::You);
        assert_eq!(estimate.coop_close_fee_sats, Some(700));
        assert_eq!(estimate.commitment_fee_sats, Some(1_100));
        assert_eq!(estimate.sweep_fee_sats, Some(700));
        assert_eq!(estimate.cpfp_fee_sats, Some(2_000));
        // The unambiguous claimable read wins over the capacity fallback.
        assert_eq!(estimate.expected_back_sats, Some(80_000));
        assert_eq!(estimate.coop_total_you_pay_sats, Some(700));
        // outbound: commitment + cpfp + sweep.
        assert_eq!(estimate.force_total_you_pay_sats, Some(1_100 + 2_000 + 700));
        assert_eq!(estimate.timelock_blocks, Some(144));
        assert_eq!(estimate.pending_htlc_count, Some(2));
        assert_eq!(estimate.is_anchor, Some(true));
    }

    #[test]
    fn ambiguous_claimable_read_falls_back_to_outbound_capacity() {
        // on_close None (ambiguous or unreadable): expected_back degrades to
        // the outbound capacity, commitment fee stays None.
        let estimate = compute_close_estimate(&CloseEstimateInputs {
            is_outbound: Some(true),
            on_close: None,
            outbound_capacity_msat: Some(79_999_999), // floors to 79_999 sats
            sweep_rate_sat_per_vb: Some(5),
            urgent_rate_sat_per_vb: Some(10),
            is_anchor: Some(true),
            ..Default::default()
        });
        assert_eq!(estimate.expected_back_sats, Some(79_999));
        assert_eq!(estimate.commitment_fee_sats, None);
        // Outbound with an unknown commitment fee: the force total is
        // withheld rather than understated (PWA estimate.ts:206-216).
        assert_eq!(estimate.force_total_you_pay_sats, None);
    }

    #[test]
    fn counterparty_funded_channel_costs_nothing_coop_and_skips_commitment() {
        let estimate = compute_close_estimate(&CloseEstimateInputs {
            is_outbound: Some(false),
            on_close: None,
            outbound_capacity_msat: Some(10_000_000),
            coop_close_sat_per_kw: Some(1_000),
            sweep_rate_sat_per_vb: Some(5),
            urgent_rate_sat_per_vb: Some(10),
            is_anchor: Some(false),
            ..Default::default()
        });
        assert_eq!(estimate.fee_payer, CloseFeePayer::Counterparty);
        // The LSP pays the coop close fee.
        assert_eq!(estimate.coop_total_you_pay_sats, Some(0));
        // Non-anchor: CPFP is exactly 0, not unknown.
        assert_eq!(estimate.cpfp_fee_sats, Some(0));
        // Inbound: no commitment fee needed for the force total — just the
        // sweep leg (CPFP is 0 for non-anchor).
        assert_eq!(estimate.force_total_you_pay_sats, Some(700));
    }

    #[test]
    fn unknown_anchor_support_keeps_cpfp_and_force_total_unknown() {
        // PWA estimate.ts:192-198: zeroing an unknown anchor flag would make
        // the force close look cheaper than it may be.
        let estimate = compute_close_estimate(&CloseEstimateInputs {
            is_outbound: Some(false),
            is_anchor: None,
            sweep_rate_sat_per_vb: Some(5),
            urgent_rate_sat_per_vb: Some(10),
            ..Default::default()
        });
        assert_eq!(estimate.cpfp_fee_sats, None);
        assert_eq!(estimate.force_total_you_pay_sats, None);
    }

    #[test]
    fn sweep_and_cpfp_estimates_floor_at_one_sat() {
        // PWA satsFromVbytes: Math.max(1, ...).
        let estimate = compute_close_estimate(&CloseEstimateInputs {
            is_anchor: Some(true),
            sweep_rate_sat_per_vb: Some(0),
            urgent_rate_sat_per_vb: Some(0),
            ..Default::default()
        });
        assert_eq!(estimate.sweep_fee_sats, Some(1));
        assert_eq!(estimate.cpfp_fee_sats, Some(1));
    }
}
