//! Close records (U10; R9 records half, R3; KTD-3, F4): persistent,
//! facts-only history of channel closes, statuses derived — never stored.
//!
//! The PWA's `zinq/src/ldk/close-records/close-record.ts` is NORMATIVE here
//! the same way KTD-2's crypto is: [`merge_close_records`] is an exact port
//! of `mergeCloseRecords` (an asymmetric, non-commutative per-field lattice),
//! pinned by merge vectors exported from the PWA's own implementation
//! (`fixtures/close_record_merge_vectors.json`). Records live in a singleton
//! channelId → record map: local KVStore first, best-effort VSS
//! `close_records` with FIELD-WISE merge on 409 (base = local, incoming =
//! remote — direction is load-bearing; `store.ts:95-115`), because blob-LWW
//! would clobber the other device's facts (its sweep txid vs our commitment
//! fee).
//!
//! [`reconcile_close_records`] is the chain-truth healer
//! (`close-records/reconcile.ts`): budgeted first-party Esplora queries,
//! funding-outspend re-checks until a close CONFIRMS (a recorded close tx is
//! only a broadcast-time claim — the counterparty's commitment can supersede
//! ours), a mempool-window exception for unconfirmed closes, and positive-
//! evidence-only completion (`resolved_unverified` when the close resolved
//! on-chain but our wallet never saw the funds).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use lightning::events::ClosureReason;
use lightning::log_error;
use lightning::util::logger::Logger as _;
use lightning::util::persist::KVStoreSync;
use lightning_persister::fs_store::FilesystemStore;
use serde::{Deserialize, Serialize};
use serde_json::{Map as JsonMap, Value};

use crate::history::{CloseRecordSource, CloseRecordSummary, CloseStatusLabel};
use crate::types::Logger;
use crate::vss::store::{BoxFuture, VssBackedStore, CLOSE_RECORDS_VSS_KEY};

/// Local KVStore namespace for the two whole-map blobs (the PWA's
/// `ldk_close_records` IDB store).
pub(crate) const CLOSE_RECORDS_PRIMARY_NAMESPACE: &str = "close_records";
pub(crate) const CLOSE_RECORDS_SECONDARY_NAMESPACE: &str = "";
/// The records map (`store.ts:33`).
pub(crate) const RECORDS_LOCAL_KEY: &str = "records";
/// The funding-txo safety-net map (`store.ts:34`). Local-only, like the PWA.
pub(crate) const FUNDING_TXOS_LOCAL_KEY: &str = "funding_txos";

/// `close-record.ts:53`.
pub(crate) const CLOSE_RECORD_SCHEMA_VERSION: u64 = 1;

/// LDK's ANTI_REORG_DELAY: final for our purposes at 6 confs
/// (`reconcile.ts:42`).
pub(crate) const FINALITY_CONFS: u32 = 6;
/// LDK's hard cap on to_self_delay — terminal-state fallback for records
/// whose actual timelock was never captured (`reconcile.ts:50`).
pub(crate) const MAX_TIMELOCK_BLOCKS: u32 = 2016;
/// Query budget per reconcile pass (`reconcile.ts:52`).
pub(crate) const MAX_QUERIES_PER_PASS: u32 = 8;

/// Safety-net record description (`reconcile.ts:143`).
pub(crate) const OFFLINE_CLOSE_REASON: &str = "Channel closed while the app was offline";

// ---------------------------------------------------------------------------
// Record shape (close-record.ts:11-51)
// ---------------------------------------------------------------------------

/// `'coop' | 'force' | 'unknown'`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseType {
    Coop,
    Force,
    Unknown,
}

impl CloseType {
    fn as_str(self) -> &'static str {
        match self {
            CloseType::Coop => "coop",
            CloseType::Force => "force",
            CloseType::Unknown => "unknown",
        }
    }
}

/// `'local' | 'remote' | 'unknown'`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Initiator {
    Local,
    Remote,
    Unknown,
}

impl Initiator {
    fn as_str(self) -> &'static str {
        match self {
            Initiator::Local => "local",
            Initiator::Remote => "remote",
            Initiator::Unknown => "unknown",
        }
    }
}

/// Per-tx role (`close-record.ts:16`). `Other` preserves role strings from
/// newer schema versions verbatim (the PWA passes unrecognized role strings
/// through untouched).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CloseTxRole {
    Closing,
    Commitment,
    AnchorCpfp,
    HtlcClaim,
    Sweep,
    Other(String),
}

impl CloseTxRole {
    fn as_str(&self) -> &str {
        match self {
            CloseTxRole::Closing => "closing",
            CloseTxRole::Commitment => "commitment",
            CloseTxRole::AnchorCpfp => "anchor_cpfp",
            CloseTxRole::HtlcClaim => "htlc_claim",
            CloseTxRole::Sweep => "sweep",
            CloseTxRole::Other(role) => role,
        }
    }

    fn from_str(role: &str) -> Self {
        match role {
            "closing" => CloseTxRole::Closing,
            "commitment" => CloseTxRole::Commitment,
            "anchor_cpfp" => CloseTxRole::AnchorCpfp,
            "htlc_claim" => CloseTxRole::HtlcClaim,
            "sweep" => CloseTxRole::Sweep,
            other => CloseTxRole::Other(other.to_string()),
        }
    }

    /// Whether this role marks the channel-closing transaction itself.
    fn is_close(&self) -> bool {
        matches!(self, CloseTxRole::Closing | CloseTxRole::Commitment)
    }
}

/// `'verified' | 'unverified'` (`close-record.ts:48`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolution {
    Verified,
    Unverified,
}

impl Resolution {
    fn as_str(self) -> &'static str {
        match self {
            Resolution::Verified => "verified",
            Resolution::Unverified => "unverified",
        }
    }
}

/// A funding outpoint in DISPLAY txid hex (Esplora's byte order).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloseOutpoint {
    pub txid: String,
    pub vout: u32,
}

/// One transaction attached to a close (`close-record.ts:18-24`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloseRecordTx {
    pub txid: String,
    pub role: CloseTxRole,
    pub fee_sats: Option<u64>,
    /// Write-once at first confirmation; live conf count derived at render.
    pub confirmed_at_height: Option<u32>,
}

/// One close record (`close-record.ts:26-51`): immutable facts only —
/// display status is computed by [`derive_close_status`], never stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloseRecord {
    pub schema_version: u64,
    pub channel_id: String,
    pub funding_txo: Option<CloseOutpoint>,
    pub close_type: CloseType,
    pub initiator: Initiator,
    /// Raw ClosureReason description, display-only pass-through.
    pub closure_reason: Option<String>,
    /// Union by txid; one batched sweep txid may appear in N records.
    pub txs: Vec<CloseRecordTx>,
    /// LDK's last-known local balance at close — an estimate, never measured.
    pub expected_amount_sats: Option<u64>,
    /// to_self_delay in blocks, captured while the channel was open.
    pub timelock_blocks: Option<u32>,
    /// Close confirm height + timelock_blocks.
    pub claimable_at_height: Option<u32>,
    /// Stable history sort key (unix ms), set at event time.
    pub created_at_ms: u64,
    /// Set-once terminal marker (unix ms). Positive evidence only.
    pub completed_at_ms: Option<u64>,
    pub resolution: Option<Resolution>,
    /// Unknown fields from newer schema versions, preserved through
    /// decode → merge → encode. Empty = absent on the wire.
    pub extras: JsonMap<String, Value>,
}

impl CloseRecord {
    /// A facts-free skeleton (the reconcile pass's `facts` template,
    /// `reconcile.ts:164-171`).
    pub(crate) fn skeleton(channel_id: &str, created_at_ms: u64) -> Self {
        Self {
            schema_version: CLOSE_RECORD_SCHEMA_VERSION,
            channel_id: channel_id.to_string(),
            funding_txo: None,
            close_type: CloseType::Unknown,
            initiator: Initiator::Unknown,
            closure_reason: None,
            txs: Vec::new(),
            expected_amount_sats: None,
            timelock_blocks: None,
            claimable_at_height: None,
            created_at_ms,
            completed_at_ms: None,
            resolution: None,
            extras: JsonMap::new(),
        }
    }

    fn has_confirmed_close_tx(&self) -> bool {
        self.txs
            .iter()
            .any(|tx| tx.role.is_close() && tx.confirmed_at_height.is_some())
    }
}

// ---------------------------------------------------------------------------
// The normative merge (close-record.ts:87-126)
// ---------------------------------------------------------------------------

/// Exact port of the PWA's `mergeCloseRecords` — `base` is the existing
/// record, `incoming` carries new facts. Identity facts are set-once (known
/// beats unknown, never downgrade, base-wins `??`); measurements
/// (`expectedAmountSats`, `claimableAtHeight`) take the incoming value when
/// present; txs union by txid with per-field fill-in preferring the EXISTING
/// side; `completedAt` set-once with `verified` absorbing `unverified`;
/// `createdAt` min; `schemaVersion` max; extras base-over-incoming.
/// Deliberately NOT commutative — direction on 409 is base = local.
pub fn merge_close_records(base: &CloseRecord, incoming: &CloseRecord) -> CloseRecord {
    // Tx union keyed by txid, base's insertion order first (JS Map parity).
    let mut txs: Vec<CloseRecordTx> = base.txs.clone();
    for tx in &incoming.txs {
        if let Some(existing) = txs.iter_mut().find(|t| t.txid == tx.txid) {
            *existing = CloseRecordTx {
                txid: existing.txid.clone(),
                role: existing.role.clone(),
                fee_sats: existing.fee_sats.or(tx.fee_sats),
                confirmed_at_height: existing.confirmed_at_height.or(tx.confirmed_at_height),
            };
        } else {
            txs.push(tx.clone());
        }
    }

    let completed_at_ms = base.completed_at_ms.or(incoming.completed_at_ms);
    let mut resolution = base.resolution.or(incoming.resolution);
    if base.resolution == Some(Resolution::Verified)
        || incoming.resolution == Some(Resolution::Verified)
    {
        resolution = Some(Resolution::Verified);
    }

    // extras: `{...incoming.extras, ...base.extras}` — base wins per key.
    let mut extras = incoming.extras.clone();
    for (key, value) in &base.extras {
        extras.insert(key.clone(), value.clone());
    }

    CloseRecord {
        schema_version: base.schema_version.max(incoming.schema_version),
        channel_id: base.channel_id.clone(),
        funding_txo: base.funding_txo.clone().or(incoming.funding_txo.clone()),
        close_type: if incoming.close_type != CloseType::Unknown {
            incoming.close_type
        } else {
            base.close_type
        },
        initiator: if incoming.initiator != Initiator::Unknown {
            incoming.initiator
        } else {
            base.initiator
        },
        closure_reason: base
            .closure_reason
            .clone()
            .or(incoming.closure_reason.clone()),
        txs,
        expected_amount_sats: incoming.expected_amount_sats.or(base.expected_amount_sats),
        timelock_blocks: base.timelock_blocks.or(incoming.timelock_blocks),
        claimable_at_height: incoming.claimable_at_height.or(base.claimable_at_height),
        created_at_ms: base.created_at_ms.min(incoming.created_at_ms),
        completed_at_ms,
        resolution,
        extras,
    }
}

// ---------------------------------------------------------------------------
// Status derivation (close-record.ts:63-77) — pure, never stored
// ---------------------------------------------------------------------------

/// Exact port of `deriveCloseStatus`, mapped onto U5's [`CloseStatusLabel`].
pub fn derive_close_status(record: &CloseRecord, current_height: Option<u32>) -> CloseStatusLabel {
    if record.completed_at_ms.is_some() {
        return if record.resolution == Some(Resolution::Unverified) {
            CloseStatusLabel::ResolvedUnverified
        } else {
            CloseStatusLabel::Complete
        };
    }
    let sweep = record.txs.iter().find(|tx| tx.role == CloseTxRole::Sweep);
    if let Some(sweep) = sweep {
        if sweep.confirmed_at_height.is_none() {
            return CloseStatusLabel::Returning;
        }
    }
    if let Some(claimable) = record.claimable_at_height {
        if current_height.is_none_or(|height| claimable > height) {
            return CloseStatusLabel::WaitingTimelock;
        }
    }
    if sweep.is_some() {
        return CloseStatusLabel::Returning;
    }
    CloseStatusLabel::Closing
}

// ---------------------------------------------------------------------------
// Serialization (close-record.ts:128-237): one codec for local and VSS.
// camelCase keys, u64 amounts as STRINGS (the PWA stores JS bigints as
// strings; JSON.stringify throws on bigint), tolerant decode with extras.
// ---------------------------------------------------------------------------

const KNOWN_RECORD_KEYS: [&str; 14] = [
    "schemaVersion",
    "channelId",
    "fundingTxo",
    "closeType",
    "initiator",
    "closureReason",
    "txs",
    "expectedAmountSats",
    "timelockBlocks",
    "claimableAtHeight",
    "createdAt",
    "completedAt",
    "resolution",
    "extras",
];

/// `serializeCloseRecord` (close-record.ts:148-160): extras first (unknown
/// fields from newer schemas survive the round-trip), then known fields;
/// absent fields omitted.
pub(crate) fn serialize_close_record(record: &CloseRecord) -> Value {
    let mut map = JsonMap::new();
    for (key, value) in &record.extras {
        map.insert(key.clone(), value.clone());
    }
    map.insert("schemaVersion".into(), record.schema_version.into());
    map.insert("channelId".into(), record.channel_id.clone().into());
    if let Some(txo) = &record.funding_txo {
        map.insert(
            "fundingTxo".into(),
            serde_json::json!({"txid": txo.txid, "vout": txo.vout}),
        );
    }
    map.insert("closeType".into(), record.close_type.as_str().into());
    map.insert("initiator".into(), record.initiator.as_str().into());
    if let Some(reason) = &record.closure_reason {
        map.insert("closureReason".into(), reason.clone().into());
    }
    let txs: Vec<Value> = record
        .txs
        .iter()
        .map(|tx| {
            let mut tx_map = JsonMap::new();
            tx_map.insert("txid".into(), tx.txid.clone().into());
            tx_map.insert("role".into(), tx.role.as_str().into());
            if let Some(fee) = tx.fee_sats {
                tx_map.insert("feeSats".into(), fee.to_string().into());
            }
            if let Some(height) = tx.confirmed_at_height {
                tx_map.insert("confirmedAtHeight".into(), height.into());
            }
            Value::Object(tx_map)
        })
        .collect();
    map.insert("txs".into(), Value::Array(txs));
    if let Some(sats) = record.expected_amount_sats {
        map.insert("expectedAmountSats".into(), sats.to_string().into());
    }
    if let Some(blocks) = record.timelock_blocks {
        map.insert("timelockBlocks".into(), blocks.into());
    }
    if let Some(height) = record.claimable_at_height {
        map.insert("claimableAtHeight".into(), height.into());
    }
    map.insert("createdAt".into(), record.created_at_ms.into());
    if let Some(at) = record.completed_at_ms {
        map.insert("completedAt".into(), at.into());
    }
    if let Some(resolution) = record.resolution {
        map.insert("resolution".into(), resolution.as_str().into());
    }
    Value::Object(map)
}

/// `toBigIntOrUndefined` (close-record.ts:162-173): string (non-empty,
/// parseable) or safe-integer number.
fn to_u64_or_none(value: &Value) -> Option<u64> {
    match value {
        Value::String(raw) if !raw.is_empty() => raw.parse().ok(),
        Value::Number(number) => number.as_u64(),
        _ => None,
    }
}

/// Tolerant decode (`deserializeCloseRecord`, close-record.ts:176-237):
/// unknown top-level fields land in `extras`; malformed sub-fields degrade
/// to their defaults; only a missing/non-string `channelId` rejects.
pub(crate) fn deserialize_close_record(raw: &Value, now_ms: u64) -> Option<CloseRecord> {
    let obj = raw.as_object()?;
    let channel_id = obj.get("channelId")?.as_str()?.to_string();

    let mut extras = JsonMap::new();
    for (key, value) in obj {
        if !KNOWN_RECORD_KEYS.contains(&key.as_str()) {
            extras.insert(key.clone(), value.clone());
        }
    }

    let mut txs = Vec::new();
    if let Some(raw_txs) = obj.get("txs").and_then(Value::as_array) {
        for raw_tx in raw_txs {
            let Some(tx) = raw_tx.as_object() else {
                continue;
            };
            let Some(txid) = tx.get("txid").and_then(Value::as_str) else {
                continue;
            };
            txs.push(CloseRecordTx {
                txid: txid.to_string(),
                role: tx
                    .get("role")
                    .and_then(Value::as_str)
                    .map(CloseTxRole::from_str)
                    .unwrap_or(CloseTxRole::Closing),
                fee_sats: tx.get("feeSats").and_then(to_u64_or_none),
                confirmed_at_height: tx
                    .get("confirmedAtHeight")
                    .and_then(Value::as_u64)
                    .map(|h| h as u32),
            });
        }
    }

    let funding_txo =
        obj.get("fundingTxo")
            .and_then(Value::as_object)
            .and_then(|txo| -> Option<CloseOutpoint> {
                Some(CloseOutpoint {
                    txid: txo.get("txid")?.as_str()?.to_string(),
                    vout: txo.get("vout")?.as_u64()? as u32,
                })
            });

    let close_type = match obj.get("closeType").and_then(Value::as_str) {
        Some("coop") => CloseType::Coop,
        Some("force") => CloseType::Force,
        _ => CloseType::Unknown,
    };
    let initiator = match obj.get("initiator").and_then(Value::as_str) {
        Some("local") => Initiator::Local,
        Some("remote") => Initiator::Remote,
        _ => Initiator::Unknown,
    };
    let resolution = match obj.get("resolution").and_then(Value::as_str) {
        Some("verified") => Some(Resolution::Verified),
        Some("unverified") => Some(Resolution::Unverified),
        _ => None,
    };

    Some(CloseRecord {
        schema_version: obj
            .get("schemaVersion")
            .and_then(Value::as_u64)
            .unwrap_or(CLOSE_RECORD_SCHEMA_VERSION),
        channel_id,
        funding_txo,
        close_type,
        initiator,
        closure_reason: obj
            .get("closureReason")
            .and_then(Value::as_str)
            .map(str::to_string),
        txs,
        expected_amount_sats: obj.get("expectedAmountSats").and_then(to_u64_or_none),
        timelock_blocks: obj
            .get("timelockBlocks")
            .and_then(Value::as_u64)
            .map(|b| b as u32),
        claimable_at_height: obj
            .get("claimableAtHeight")
            .and_then(Value::as_u64)
            .map(|h| h as u32),
        created_at_ms: obj
            .get("createdAt")
            .and_then(Value::as_u64)
            .unwrap_or(now_ms),
        completed_at_ms: obj.get("completedAt").and_then(Value::as_u64),
        resolution,
        extras,
    })
}

/// Whole-map codec (`store.ts:64-78`): `{channelId: serialized record}`.
pub(crate) fn encode_records_map(records: &HashMap<String, CloseRecord>) -> Value {
    let mut map = JsonMap::new();
    // Deterministic key order so identical maps encode to identical bytes.
    let mut channel_ids: Vec<&String> = records.keys().collect();
    channel_ids.sort();
    for channel_id in channel_ids {
        map.insert(
            channel_id.clone(),
            serialize_close_record(&records[channel_id]),
        );
    }
    Value::Object(map)
}

pub(crate) fn decode_records_map(raw: &Value, now_ms: u64) -> HashMap<String, CloseRecord> {
    let mut map = HashMap::new();
    if let Some(obj) = raw.as_object() {
        for value in obj.values() {
            if let Some(record) = deserialize_close_record(value, now_ms) {
                map.insert(record.channel_id.clone(), record);
            }
        }
    }
    map
}

// ---------------------------------------------------------------------------
// Closure-reason classification (closure-reason.ts) — exhaustive mapping
// ---------------------------------------------------------------------------

/// The PWA's `ClosureClassification` (`closure-reason.ts:20-30`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ClosureClassification {
    pub description: String,
    pub close_type: CloseType,
    pub initiator: Initiator,
    /// False when no on-chain closing tx exists (unfunded/abandoned) — those
    /// never create a close record.
    pub has_onchain_tx: bool,
}

/// Port of `classifyClosureReason` — the copy strings are PWA-exact.
pub(crate) fn classify_closure_reason(reason: &ClosureReason) -> ClosureClassification {
    let class = |description: &str, close_type, initiator, has_onchain_tx| ClosureClassification {
        description: description.to_string(),
        close_type,
        initiator,
        has_onchain_tx,
    };
    match reason {
        ClosureReason::LegacyCooperativeClosure => class(
            "Cooperative close",
            CloseType::Coop,
            Initiator::Unknown,
            true,
        ),
        ClosureReason::LocallyInitiatedCooperativeClosure => {
            class("Cooperative close", CloseType::Coop, Initiator::Local, true)
        }
        ClosureReason::CounterpartyInitiatedCooperativeClosure => class(
            "Cooperative close (initiated by peer)",
            CloseType::Coop,
            Initiator::Remote,
            true,
        ),
        ClosureReason::HolderForceClosed { .. } => class(
            "Force closed by you",
            CloseType::Force,
            Initiator::Local,
            true,
        ),
        ClosureReason::CounterpartyForceClosed { .. } => class(
            "Counterparty force closed",
            CloseType::Force,
            Initiator::Remote,
            true,
        ),
        ClosureReason::CommitmentTxConfirmed => class(
            "Commitment transaction confirmed",
            CloseType::Force,
            Initiator::Remote,
            true,
        ),
        ClosureReason::HTLCsTimedOut { .. } => class(
            "Force closed to resolve timed-out payments",
            CloseType::Force,
            Initiator::Local,
            true,
        ),
        ClosureReason::ProcessingError { .. } => {
            class("Processing error", CloseType::Force, Initiator::Local, true)
        }
        ClosureReason::OutdatedChannelManager => class(
            "Outdated channel manager",
            CloseType::Force,
            Initiator::Local,
            true,
        ),
        ClosureReason::PeerFeerateTooLow { .. } => class(
            "Peer feerate too low",
            CloseType::Force,
            Initiator::Local,
            true,
        ),
        ClosureReason::DisconnectedPeer => class(
            "Peer disconnected",
            CloseType::Unknown,
            Initiator::Unknown,
            false,
        ),
        ClosureReason::FundingTimedOut => class(
            "Funding timed out",
            CloseType::Unknown,
            Initiator::Unknown,
            false,
        ),
        ClosureReason::CounterpartyCoopClosedUnfundedChannel => class(
            "Counterparty closed unfunded channel",
            CloseType::Unknown,
            Initiator::Remote,
            false,
        ),
        ClosureReason::LocallyCoopClosedUnfundedChannel => class(
            "Closed unfunded channel",
            CloseType::Unknown,
            Initiator::Local,
            false,
        ),
        ClosureReason::FundingBatchClosure => class(
            "Funding batch closure",
            CloseType::Unknown,
            Initiator::Unknown,
            false,
        ),
        // The match is exhaustive on the pinned LDK 0.2.4 — a future LDK
        // upgrade adding variants fails compilation here, forcing the mapping
        // decision (the PWA's runtime fallback is "Channel closed", unknown,
        // unknown, track-when-funding-txo-exists).
    }
}

// ---------------------------------------------------------------------------
// Typed errors
// ---------------------------------------------------------------------------

/// Typed close-record store failures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CloseRecordsError {
    /// The whole-map blob failed to serialize or write locally.
    Persist { detail: String },
}

impl std::fmt::Display for CloseRecordsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CloseRecordsError::Persist { detail } => {
                write!(f, "failed to persist the close-records map: {detail}")
            }
        }
    }
}

impl std::error::Error for CloseRecordsError {}

// ---------------------------------------------------------------------------
// Store (store.ts): one owner, in-memory sync read model, local-first + VSS
// ---------------------------------------------------------------------------

/// Safety-net map entry (`store.ts:42-44`): funding outpoint + to_self_delay,
/// both captured at `ChannelPending` (unreadable after close).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FundingTxoEntry {
    pub txid: String,
    pub vout: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timelock_blocks: Option<u32>,
}

/// The close-record store: in-memory map (the SYNCHRONOUS read model — the
/// BumpTransaction handler must never block on storage to look up close
/// context), local KVStore mirror, best-effort VSS singleton with field-wise
/// merge on 409. Implements U5's [`CloseRecordSource`] for the activity feed.
pub(crate) struct CloseRecordStore {
    records: Mutex<HashMap<String, CloseRecord>>,
    funding_txos: Mutex<HashMap<String, FundingTxoEntry>>,
    kv_store: Arc<FilesystemStore>,
    vss: Mutex<Option<Arc<VssBackedStore>>>,
    /// Ephemeral last-seen tip height (set by reconcile, `store.ts:229`) —
    /// lets detail views derive live conf counts without extra requests.
    last_tip_height: Mutex<Option<u32>>,
    logger: Arc<Logger>,
}

impl CloseRecordStore {
    /// Loads both maps from the local mirror. Corruption degrades to empty
    /// with a log (records drive UI + recovery exits; reconcile heals).
    pub(crate) fn new(kv_store: Arc<FilesystemStore>, logger: Arc<Logger>) -> Self {
        let read_map = |key: &str| -> Option<Value> {
            let bytes = kv_store
                .read(
                    CLOSE_RECORDS_PRIMARY_NAMESPACE,
                    CLOSE_RECORDS_SECONDARY_NAMESPACE,
                    key,
                )
                .ok()?;
            serde_json::from_slice(&bytes).ok()
        };
        let now_ms = crate::util::now_ms();
        let records = read_map(RECORDS_LOCAL_KEY)
            .map(|raw| decode_records_map(&raw, now_ms))
            .unwrap_or_default();
        let funding_txos = read_map(FUNDING_TXOS_LOCAL_KEY)
            .and_then(|raw| serde_json::from_value(raw).ok())
            .unwrap_or_default();
        Self {
            records: Mutex::new(records),
            funding_txos: Mutex::new(funding_txos),
            kv_store,
            vss: Mutex::new(None),
            last_tip_height: Mutex::new(None),
            logger,
        }
    }

    /// Attaches the VSS half at node start (the store itself outlives the
    /// node, like the payment store).
    pub(crate) fn attach_vss(&self, vss: Arc<VssBackedStore>) {
        *self.vss.lock().unwrap() = Some(vss);
    }

    /// Detaches at node stop so no VSS write is scheduled onto a dead
    /// runtime. Local persistence keeps working.
    pub(crate) fn detach_vss(&self) {
        *self.vss.lock().unwrap() = None;
    }

    /// Drops all in-memory and cached state. Called by U4's restore after the
    /// store dir was replaced: the maps belong to the REPLACED wallet.
    pub(crate) fn reset(&self) {
        self.records.lock().unwrap().clear();
        self.funding_txos.lock().unwrap().clear();
        *self.last_tip_height.lock().unwrap() = None;
    }

    /// Synchronous read model — safe from the LDK event handler.
    pub(crate) fn get(&self, channel_id: &str) -> Option<CloseRecord> {
        self.records.lock().unwrap().get(channel_id).cloned()
    }

    /// Snapshot, newest first (`store.ts:54-56`).
    pub(crate) fn snapshot(&self) -> Vec<CloseRecord> {
        let mut records: Vec<CloseRecord> =
            self.records.lock().unwrap().values().cloned().collect();
        records.sort_by_key(|record| std::cmp::Reverse(record.created_at_ms));
        records
    }

    /// Create-or-merge (`store.ts:192-197`): the in-memory map is updated
    /// SYNCHRONOUSLY (BumpTransaction fires right after ChannelClosed in the
    /// same drain and must see the record); persistence follows. Direction:
    /// base = existing, incoming = new facts. Local persist failure is
    /// logged, never a replay — the funding-txo safety net + reconcile heal.
    pub(crate) fn upsert(self: &Arc<Self>, incoming: CloseRecord) {
        {
            let mut records = self.records.lock().unwrap();
            let merged = match records.get(&incoming.channel_id) {
                Some(existing) => merge_close_records(existing, &incoming),
                None => incoming.clone(),
            };
            records.insert(incoming.channel_id.clone(), merged);
        }
        let bytes = self.encode_records();
        if let Err(e) = self.persist_records_locally(bytes.clone()) {
            log_error!(self.logger, "Close-record local persist failed: {e}");
        }
        self.sync_vss(bytes);
    }

    /// Encodes the records map once — the same bytes back the local mirror
    /// write, the VSS put, and the 409-merge return value.
    fn encode_records(&self) -> Vec<u8> {
        serde_json::to_vec(&encode_records_map(&self.records.lock().unwrap()))
            .expect("string-keyed records map serializes")
    }

    /// Writes the encoded records map to the local mirror.
    fn persist_records_locally(&self, bytes: Vec<u8>) -> Result<(), CloseRecordsError> {
        self.kv_store
            .write(
                CLOSE_RECORDS_PRIMARY_NAMESPACE,
                CLOSE_RECORDS_SECONDARY_NAMESPACE,
                RECORDS_LOCAL_KEY,
                bytes,
            )
            .map_err(|e| CloseRecordsError::Persist {
                detail: e.to_string(),
            })
    }

    /// Schedules the best-effort VSS write with field-wise merge on 409
    /// (`store.ts:95-115`): the merge callback folds the remote map into the
    /// LOCAL store (base = local) and returns the merged bytes.
    fn sync_vss(self: &Arc<Self>, bytes: Vec<u8>) {
        let Some(vss) = self.vss.lock().unwrap().clone() else {
            return;
        };
        let store = Arc::downgrade(self);
        vss.put_with_merge(
            CLOSE_RECORDS_VSS_KEY,
            bytes,
            Arc::new(move |remote_bytes: &[u8]| match store.upgrade() {
                Some(store) => store.absorb_remote(remote_bytes),
                None => remote_bytes.to_vec(),
            }),
        );
    }

    /// Folds a remote map into the local one — base = LOCAL, incoming =
    /// remote (`store.ts:80-85` via `mergeMapInto`) — persists locally, and
    /// returns the merged bytes. Used by both the 409 path and init seeding.
    pub(crate) fn absorb_remote(&self, remote_bytes: &[u8]) -> Vec<u8> {
        let now_ms = crate::util::now_ms();
        let remote_map = serde_json::from_slice::<Value>(remote_bytes)
            .map(|raw| decode_records_map(&raw, now_ms))
            .unwrap_or_default();
        {
            let mut records = self.records.lock().unwrap();
            for (channel_id, incoming) in remote_map {
                let merged = match records.get(&channel_id) {
                    Some(existing) => merge_close_records(existing, &incoming),
                    None => incoming,
                };
                records.insert(channel_id, merged);
            }
        }
        let bytes = self.encode_records();
        if let Err(e) = self.persist_records_locally(bytes.clone()) {
            log_error!(self.logger, "Close-record local persist failed: {e}");
        }
        bytes
    }

    /// Init seeding (`store.ts:147-164`): merge the remote snapshot in (the
    /// version was already recorded by `fetch_versioned`).
    pub(crate) fn seed_from_remote(&self, remote_bytes: &[u8]) {
        let _ = self.absorb_remote(remote_bytes);
    }

    // --- funding-txo safety net (store.ts:199-225) -------------------------

    /// Persists channelId → funding outpoint (+ timelock) while the channel
    /// is open. Idempotent on identical entries.
    pub(crate) fn record_funding_txo(&self, channel_id: &str, entry: FundingTxoEntry) {
        {
            let mut map = self.funding_txos.lock().unwrap();
            if map.get(channel_id) == Some(&entry) {
                return;
            }
            map.insert(channel_id.to_string(), entry);
        }
        self.persist_funding_txos();
    }

    pub(crate) fn funding_txo_map(&self) -> HashMap<String, FundingTxoEntry> {
        self.funding_txos.lock().unwrap().clone()
    }

    pub(crate) fn remove_funding_txo(&self, channel_id: &str) {
        {
            let mut map = self.funding_txos.lock().unwrap();
            if map.remove(channel_id).is_none() {
                return;
            }
        }
        self.persist_funding_txos();
    }

    fn persist_funding_txos(&self) {
        let bytes = serde_json::to_vec(&*self.funding_txos.lock().unwrap())
            .expect("string-keyed map serializes");
        if let Err(e) = self.kv_store.write(
            CLOSE_RECORDS_PRIMARY_NAMESPACE,
            CLOSE_RECORDS_SECONDARY_NAMESPACE,
            FUNDING_TXOS_LOCAL_KEY,
            bytes,
        ) {
            log_error!(self.logger, "Funding-txo map persist failed: {e}");
        }
    }

    // --- tip height (store.ts:227-238) --------------------------------------

    pub(crate) fn set_last_tip_height(&self, height: u32) {
        *self.last_tip_height.lock().unwrap() = Some(height);
    }

    pub(crate) fn last_tip_height(&self) -> Option<u32> {
        *self.last_tip_height.lock().unwrap()
    }
}

impl CloseRecordSource for CloseRecordStore {
    /// U5's activity seam: one summary per record with the derived status and
    /// ALL attached txids (commitment/sweep/...) as the absorption set —
    /// including sweep txids attributed by U11's `SweepResult` (KTD-7).
    fn summaries(&self) -> Vec<CloseRecordSummary> {
        let tip = self.last_tip_height();
        self.snapshot()
            .into_iter()
            .map(|record| CloseRecordSummary {
                channel_id: record.channel_id.clone(),
                created_at_ms: record.created_at_ms,
                expected_amount_sats: record.expected_amount_sats,
                status: derive_close_status(&record, tip),
                absorbed_txids: record.txs.iter().map(|tx| tx.txid.clone()).collect(),
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Event-side signals (signals.ts)
// ---------------------------------------------------------------------------

/// `handleCloseSignal`'s `channel_closed` branch (signals.ts:37-70): builds
/// the record from the drained ChannelClosed facts + the safety-net map.
/// Idempotent under replay — duplicate facts merge to no-ops.
pub(crate) fn on_channel_closed(
    store: &Arc<CloseRecordStore>,
    channel_id_hex: &str,
    classification: &ClosureClassification,
    event_funding_txo: Option<CloseOutpoint>,
    last_local_balance_sats: Option<u64>,
    now_ms: u64,
) {
    let map_entry = store.funding_txo_map().get(channel_id_hex).cloned();
    let funding_txo = event_funding_txo.or_else(|| {
        map_entry.as_ref().map(|entry| CloseOutpoint {
            txid: entry.txid.clone(),
            vout: entry.vout,
        })
    });
    // to_self_delay is unreadable once the channel is gone — the safety-net
    // map (captured at ChannelPending) is the only source at close time.
    let timelock_blocks = map_entry.as_ref().and_then(|entry| entry.timelock_blocks);

    // No on-chain close tx and nothing to watch → no record; drop the
    // safety-net entry so reconciliation doesn't resurrect it.
    if !classification.has_onchain_tx && store.get(channel_id_hex).is_none() {
        store.remove_funding_txo(channel_id_hex);
        return;
    }

    let mut record = CloseRecord::skeleton(channel_id_hex, now_ms);
    record.funding_txo = funding_txo;
    record.close_type = classification.close_type;
    record.initiator = classification.initiator;
    record.closure_reason = Some(classification.description.clone());
    record.expected_amount_sats = last_local_balance_sats;
    record.timelock_blocks = timelock_blocks;
    store.upsert(record);
    // The record now carries the funding txo; the map entry is only needed
    // for closes that never produced a record.
    store.remove_funding_txo(channel_id_hex);
}

/// `handleCloseSignal`'s `commitment_broadcast` branch (signals.ts:73-83):
/// the anchor CPFP path handed us the actual commitment tx — attach txid +
/// pre-committed fee. Merge-idempotent under event replay.
pub(crate) fn on_commitment_broadcast(
    store: &Arc<CloseRecordStore>,
    channel_id_hex: &str,
    txid: &str,
    fee_sats: u64,
    now_ms: u64,
) {
    let mut record = CloseRecord::skeleton(channel_id_hex, now_ms);
    record.close_type = CloseType::Force;
    record.txs.push(CloseRecordTx {
        txid: txid.to_string(),
        role: CloseTxRole::Commitment,
        fee_sats: Some(fee_sats),
        confirmed_at_height: None,
    });
    store.upsert(record);
}

/// `recordSweepResult` (signals.ts:96-115), the U11 seam: attaches a
/// broadcast sweep txid to every record whose channel contributed an output
/// (attribution by the channelId persisted with each descriptor — never by
/// "the sweep my event triggered").
pub(crate) fn record_sweep_tx(
    store: &Arc<CloseRecordStore>,
    txid: &str,
    channel_id_hexes: &HashSet<String>,
    now_ms: u64,
) {
    for channel_id_hex in channel_id_hexes {
        let mut record = CloseRecord::skeleton(channel_id_hex, now_ms);
        record.txs.push(CloseRecordTx {
            txid: txid.to_string(),
            role: CloseTxRole::Sweep,
            fee_sats: None,
            confirmed_at_height: None,
        });
        store.upsert(record);
    }
}

// ---------------------------------------------------------------------------
// Chain-truth reconciliation (reconcile.ts)
// ---------------------------------------------------------------------------

/// The chain queries the reconcile pass needs, over the FIRST-PARTY Esplora
/// only (`reconcile.ts:56-59`: recurring outspend polling of channel
/// outpoints through a third party would leak the user's IP + channel set).
pub(crate) trait ChainTruth: Send + Sync {
    /// Current tip height. NOT counted against the query budget (the PWA
    /// gets its tip from the sync callback for free).
    fn tip_height(&self) -> BoxFuture<'_, Result<u32, String>>;
    /// The txid spending `txid:vout`, if any (mempool spends included).
    fn outspend<'a>(
        &'a self,
        txid: &'a str,
        vout: u32,
    ) -> BoxFuture<'a, Result<Option<String>, String>>;
    /// The confirmation height of `txid`, `None` while unconfirmed.
    fn tx_confirmed_height<'a>(
        &'a self,
        txid: &'a str,
    ) -> BoxFuture<'a, Result<Option<u32>, String>>;
}

/// Wallet receipt evidence (`reconcile.ts:69-76`): whether `txid` is
/// confirmed in OUR bdk wallet. Absence is never evidence.
pub(crate) trait WalletReceipts: Send + Sync {
    fn tx_confirmed_in_wallet(&self, txid: &str) -> bool;
}

fn confirmations(tip_height: u32, confirmed_at_height: u32) -> u32 {
    tip_height.saturating_sub(confirmed_at_height) + 1
}

/// One reconcile pass (`reconcileCloseRecords`, reconcile.ts:91-332). Runs on
/// the node's sync tick; the steady state with no pending closes costs zero
/// network. Budgeted to [`MAX_QUERIES_PER_PASS`] Esplora queries. Esplora
/// errors leave records stale for the next pass; they never complete
/// anything — stale is safe, wrong "complete" is not.
pub(crate) async fn reconcile_close_records(
    store: &Arc<CloseRecordStore>,
    chain: &dyn ChainTruth,
    wallet: &dyn WalletReceipts,
    open_channel_ids: &HashSet<String>,
    pending_sweep_channels: impl Fn() -> HashSet<String>,
    now_ms: u64,
    logger: &Arc<Logger>,
) {
    let pending_records: Vec<CloseRecord> = store
        .snapshot()
        .into_iter()
        .filter(|record| record.completed_at_ms.is_none())
        .collect();
    let funding_map = store.funding_txo_map();
    if pending_records.is_empty() && funding_map.is_empty() {
        return; // Steady state: zero network work.
    }
    // U11: channels with un-swept outputs block completion. Resolved lazily
    // — only a pass that got past the steady-state check pays the sweep
    // store's KVStore list+read.
    let pending_sweep_channels = pending_sweep_channels();

    // Tip fetch (uncounted): the PWA receives the tip from its sync callback.
    let tip_height = match chain.tip_height().await {
        Ok(height) => height,
        Err(e) => {
            // Pass-level failure: records stay stale and heal later.
            log_error!(logger, "Close-record reconcile aborted: {e}");
            return;
        }
    };
    let tip_changed = store.last_tip_height() != Some(tip_height);
    store.set_last_tip_height(tip_height);

    // Mempool-window exception (reconcile.ts:99-113): while a record has no
    // CONFIRMED closing tx, its funding outspend is checked every tick — a
    // recorded-but-unconfirmed commitment may be superseded by the
    // counterparty's. Everything else moves only on a new block.
    let undiscovered_exists = pending_records
        .iter()
        .any(|record| record.funding_txo.is_some() && !record.has_confirmed_close_tx());
    if !tip_changed && !undiscovered_exists {
        return;
    }

    let mut budget = MAX_QUERIES_PER_PASS;

    // 1. Safety-net records for channels that vanished recordless
    //    (reconcile.ts:125-151).
    if tip_changed && !funding_map.is_empty() {
        for (channel_id, txo) in &funding_map {
            if open_channel_ids.contains(channel_id) {
                continue;
            }
            if store.get(channel_id).is_some() {
                store.remove_funding_txo(channel_id);
                continue;
            }
            let mut record = CloseRecord::skeleton(channel_id, now_ms);
            record.funding_txo = Some(CloseOutpoint {
                txid: txo.txid.clone(),
                vout: txo.vout,
            });
            record.closure_reason = Some(OFFLINE_CLOSE_REASON.to_string());
            record.timelock_blocks = txo.timelock_blocks;
            store.upsert(record);
            store.remove_funding_txo(channel_id);
        }
    }

    let to_process: Vec<CloseRecord> = store
        .snapshot()
        .into_iter()
        .filter(|record| record.completed_at_ms.is_none())
        .collect();
    if to_process.is_empty() {
        return;
    }

    for record in &to_process {
        // Per-record isolation: an Esplora ERROR skips the record (stale,
        // healed next pass) — it must never read as "no spends".
        if let Err(e) = reconcile_one_record(
            store,
            chain,
            wallet,
            record,
            tip_changed,
            tip_height,
            &pending_sweep_channels,
            now_ms,
            &mut budget,
        )
        .await
        {
            log_error!(
                logger,
                "Reconcile: record {} skipped: {e}",
                &record.channel_id[..record.channel_id.len().min(8)]
            );
        }
        if budget == 0 {
            break;
        }
    }
}

/// Consumes one budget unit and runs the chain query; an exhausted budget
/// reads as `None` (never as evidence — the record stays stale and heals on
/// a later pass). The budget is decremented BEFORE the query is awaited.
async fn with_budget<T, F, Fut>(budget: &mut u32, query: F) -> Result<Option<T>, String>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<Option<T>, String>>,
{
    if *budget == 0 {
        return Ok(None);
    }
    *budget -= 1;
    query().await
}

/// Steps (a)-(c) for one record (reconcile.ts:160-324).
#[allow(clippy::too_many_arguments)]
async fn reconcile_one_record(
    store: &Arc<CloseRecordStore>,
    chain: &dyn ChainTruth,
    wallet: &dyn WalletReceipts,
    record: &CloseRecord,
    tip_changed: bool,
    tip_height: u32,
    pending_sweep_channels: &HashSet<String>,
    now_ms: u64,
    budget: &mut u32,
) -> Result<(), String> {
    let mut facts = CloseRecord::skeleton(&record.channel_id, record.created_at_ms);
    let mut changed = false;

    // (a) Discover the closing tx from the funding outspend. Re-checked until
    // a KNOWN close tx has CONFIRMED — a record may hold only our own
    // broadcast commitment while the counterparty's superseded it. The
    // confirmed funding spend is ground truth; the merge unions by txid, so
    // re-discovering an already-recorded tx just fills its height in.
    let undiscovered_txo = record
        .funding_txo
        .as_ref()
        .filter(|_| !record.has_confirmed_close_tx());
    if let Some(txo) = undiscovered_txo {
        let spend = with_budget(budget, || chain.outspend(&txo.txid, txo.vout)).await?;
        if let Some(spender_txid) = spend {
            let status = with_budget(budget, || chain.tx_confirmed_height(&spender_txid)).await?;
            facts.txs.push(CloseRecordTx {
                txid: spender_txid,
                role: if record.close_type == CloseType::Coop {
                    CloseTxRole::Closing
                } else {
                    CloseTxRole::Commitment
                },
                fee_sats: None,
                confirmed_at_height: status,
            });
            changed = true;
        }
    }

    // (b) Write-once confirmation heights for known txs (new-tip only).
    if tip_changed {
        for tx in &record.txs {
            if tx.confirmed_at_height.is_some() {
                continue;
            }
            let status = with_budget(budget, || chain.tx_confirmed_height(&tx.txid)).await?;
            if let Some(height) = status {
                facts.txs.push(CloseRecordTx {
                    confirmed_at_height: Some(height),
                    ..tx.clone()
                });
                changed = true;
            }
        }
    }

    // (b2) Derive the timelock expiry once the close tx confirms. Skipped for
    // coop closes AND remote-initiated closes: to_self_delay encumbers only
    // the BROADCASTER's to_local output (reconcile.ts:214-237).
    if record.claimable_at_height.is_none()
        && record.close_type != CloseType::Coop
        && record.initiator != Initiator::Remote
    {
        if let Some(timelock) = record.timelock_blocks {
            let close_confirmed_at = facts
                .txs
                .iter()
                .chain(record.txs.iter())
                .find(|tx| tx.role.is_close() && tx.confirmed_at_height.is_some())
                .and_then(|tx| tx.confirmed_at_height);
            if let Some(height) = close_confirmed_at {
                facts.claimable_at_height = Some(height + timelock);
                changed = true;
            }
        }
    }

    if changed {
        store.upsert(facts.clone());
    }

    // (c) Positive-evidence completion (new-tip only).
    if !tip_changed {
        return Ok(());
    }
    let current = store
        .get(&record.channel_id)
        .unwrap_or_else(|| record.clone());
    // Un-swept outputs pending for this channel always block completion — a
    // partial sweep's receipt must not complete the record early.
    if pending_sweep_channels.contains(&current.channel_id) {
        return Ok(());
    }

    let deep_conf = |height: Option<u32>| {
        height.is_some_and(|height| confirmations(tip_height, height) >= FINALITY_CONFS)
    };
    let close_tx = current.txs.iter().find(|tx| tx.role.is_close());
    let close_final = deep_conf(close_tx.and_then(|tx| tx.confirmed_at_height));
    // 'commitment' is a valid receipt role: safety-net records label the
    // discovered close tx 'commitment' even when it was a coop close paying
    // this wallet directly; the wallet check is the real gate — a force-close
    // commitment never pays the bdk wallet (reconcile.ts:252-259).
    let receipt_tx = current.txs.iter().find(|tx| {
        matches!(
            tx.role,
            CloseTxRole::Sweep | CloseTxRole::Closing | CloseTxRole::Commitment
        ) && deep_conf(tx.confirmed_at_height)
            && wallet.tx_confirmed_in_wallet(&tx.txid)
    });

    // Receipt evidence is checked BEFORE the timelock gate: funds deeply
    // confirmed in our own wallet are positive proof regardless of any
    // (possibly phantom) derived timelock.
    if receipt_tx.is_some() {
        let mut completion = facts.clone();
        completion.txs = Vec::new();
        completion.completed_at_ms = Some(now_ms);
        completion.resolution = Some(Resolution::Verified);
        store.upsert(completion);
        return Ok(());
    }

    // Remaining (receipt-less) outcomes must respect the timelock.
    let claim_gate = current
        .claimable_at_height
        .is_none_or(|claimable| claimable <= tip_height);
    if !claim_gate {
        return Ok(());
    }

    if current.expected_amount_sats == Some(0) && close_final {
        // Nothing to receive — the deeply-confirmed close is the whole story.
        let mut completion = facts.clone();
        completion.txs = Vec::new();
        completion.completed_at_ms = Some(now_ms);
        completion.resolution = Some(Resolution::Verified);
        store.upsert(completion);
    } else if close_final
        && (current.close_type == CloseType::Coop
            || current
                .claimable_at_height
                .is_some_and(|claimable| claimable + FINALITY_CONFS <= tip_height)
            // Timelock never captured: fall back to the maximum possible
            // to_self_delay so the record still terminates in bounded time.
            || close_tx.and_then(|tx| tx.confirmed_at_height).is_some_and(
                |height| height + MAX_TIMELOCK_BLOCKS + FINALITY_CONFS <= tip_height,
            ))
    {
        // Close resolved on-chain but our wallet never saw the funds arrive
        // (e.g. swept on a device we can't see). Terminal, rendered
        // distinctly — never laundered into "complete".
        let mut completion = facts.clone();
        completion.txs = Vec::new();
        completion.completed_at_ms = Some(now_ms);
        completion.resolution = Some(Resolution::Unverified);
        store.upsert(completion);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::{CoreEvent, EventSink};
    use crate::vss::store::{RetryTuning, VssTransport};
    use crate::vss::test_support::MockTransport;
    use std::collections::BTreeSet;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;

    /// PWA-exported merge vectors: generated by running zinq's REAL
    /// `mergeCloseRecords` (`close-record.ts:87-126`) over the cases from
    /// `close-record.test.ts` plus generated vectors for the lattice rules
    /// its tests don't pin (non-commutativity witness, base-wins `??`
    /// fields, extras preference). See the per-vector comments below for the
    /// PWA test line numbers each one transcribes.
    const MERGE_VECTORS: &str = include_str!("../fixtures/close_record_merge_vectors.json");

    fn vectors() -> Value {
        serde_json::from_str(MERGE_VECTORS).unwrap()
    }

    fn decode(value: &Value) -> CloseRecord {
        deserialize_close_record(value, 999_999_999).expect("vector records decode")
    }

    fn merge_values(base: &Value, incoming: &Value) -> Value {
        serialize_close_record(&merge_close_records(&decode(base), &decode(incoming)))
    }

    // ---------- merge vectors (normative — plan U10) ----------

    /// Every (base, incoming) → merged triple exported from the PWA must
    /// reproduce byte-equivalent wire JSON through the Rust port.
    /// Transcribed sources: `tx_union_fill_in` close-record.test.ts:24-37;
    /// `known_beats_unknown` :39-45; `created_at_min_a`/`_b` :47-54;
    /// `completed_at_set_once_verified_absorbs` :56-62;
    /// `measurements_incoming_identity_base` :76-91. Node-generated (no PWA
    /// test covers them): `base_wins_nullish_and_schema_max`,
    /// `extras_base_over_incoming`,
    /// `resolution_verified_precedence_no_completed`.
    #[test]
    fn merge_vectors_from_the_pwa_reproduce_exactly() {
        let vectors = vectors();
        let simple = [
            "tx_union_fill_in",
            "known_beats_unknown",
            "created_at_min_a",
            "created_at_min_b",
            "completed_at_set_once_verified_absorbs",
            "measurements_incoming_identity_base",
            "base_wins_nullish_and_schema_max",
            "extras_base_over_incoming",
            "resolution_verified_precedence_no_completed",
        ];
        for name in simple {
            let vector = &vectors[name];
            assert_eq!(
                merge_values(&vector["base"], &vector["incoming"]),
                vector["merged"],
                "vector {name} diverged from the PWA's mergeCloseRecords"
            );
        }
    }

    /// close-record.test.ts:64-74 — duplicate merges are no-ops on stored
    /// facts (what makes event replay idempotent without a state machine).
    #[test]
    fn merge_is_idempotent_per_the_pwa_vector() {
        let vector = &vectors()["idempotency"];
        let once = merge_values(&vector["base"], &vector["base"]);
        assert_eq!(once, vector["once"]);
        let twice = serialize_close_record(&merge_close_records(
            &decode(&vector["once"]),
            &decode(&vector["base"]),
        ));
        assert_eq!(twice, vector["twice"]);
        assert_eq!(once, twice);
    }

    /// Node-generated witness: the lattice is deliberately asymmetric —
    /// merge(a,b) != merge(b,a) (base-wins closureReason/fundingTxo/tx
    /// subfields vs incoming-wins expectedAmountSats). Direction on 409 is
    /// therefore load-bearing: base = LOCAL.
    #[test]
    fn merge_is_not_commutative_matching_the_pwa_witness() {
        let vector = &vectors()["non_commutativity_witness"];
        let a_then_b = merge_values(&vector["a"], &vector["b"]);
        let b_then_a = merge_values(&vector["b"], &vector["a"]);
        assert_eq!(a_then_b, vector["a_then_b"]);
        assert_eq!(b_then_a, vector["b_then_a"]);
        assert_ne!(a_then_b, b_then_a, "the witness must actually witness");
    }

    // ---------- serialization (close-record.test.ts:94-131) ----------

    fn record(mutate: impl FnOnce(&mut CloseRecord)) -> CloseRecord {
        let mut record = CloseRecord::skeleton("ab", 1_000);
        mutate(&mut record);
        record
    }

    /// close-record.test.ts:95-109 — round-trips through JSON with amounts
    /// as strings (the VSS path is JSON; bigints must be strings).
    #[test]
    fn serialization_round_trips_with_bigints_as_strings() {
        let original = record(|r| {
            r.close_type = CloseType::Force;
            r.funding_txo = Some(CloseOutpoint {
                txid: "f0".into(),
                vout: 1,
            });
            r.txs = vec![CloseRecordTx {
                txid: "t1".into(),
                role: CloseTxRole::Sweep,
                fee_sats: Some(123),
                confirmed_at_height: Some(7),
            }];
            r.expected_amount_sats = Some(480_000);
            r.timelock_blocks = Some(144);
            r.claimable_at_height = Some(900_000);
            r.completed_at_ms = Some(5_000);
            r.resolution = Some(Resolution::Verified);
        });
        let wire = serialize_close_record(&original);
        assert_eq!(wire["txs"][0]["feeSats"], "123", "feeSats must be a string");
        assert_eq!(wire["expectedAmountSats"], "480000");
        assert_eq!(wire["claimableAtHeight"], 900_000, "heights stay numbers");
        let json = serde_json::to_string(&wire).unwrap();
        let decoded = deserialize_close_record(&serde_json::from_str(&json).unwrap(), 0).unwrap();
        assert_eq!(decoded, original);
    }

    /// close-record.test.ts:111-123 — unknown fields from newer schema
    /// versions survive decode → merge → encode.
    #[test]
    fn extras_survive_decode_merge_encode() {
        let mut wire = serialize_close_record(&record(|_| {}));
        wire["schemaVersion"] = 2.into();
        wire["futureField"] = serde_json::json!({"nested": true});
        let decoded = deserialize_close_record(&wire, 0).expect("future schema still decodes");
        let merged = merge_close_records(
            &decoded,
            &record(|r| {
                r.txs = vec![CloseRecordTx {
                    txid: "t9".into(),
                    role: CloseTxRole::Sweep,
                    fee_sats: None,
                    confirmed_at_height: None,
                }];
            }),
        );
        let re_encoded = serialize_close_record(&merged);
        assert_eq!(
            re_encoded["futureField"],
            serde_json::json!({"nested": true})
        );
        assert_eq!(re_encoded["schemaVersion"], 2);
    }

    /// close-record.test.ts:125-130 — tolerant decode.
    #[test]
    fn decode_tolerates_garbage_input() {
        assert!(deserialize_close_record(&Value::Null, 0).is_none());
        assert!(deserialize_close_record(&"nope".into(), 0).is_none());
        assert!(deserialize_close_record(&serde_json::json!({}), 0).is_none());
        assert!(deserialize_close_record(
            &serde_json::json!({"channelId": "ab", "txs": "not-an-array"}),
            0
        )
        .is_some());
    }

    /// Unrecognized role strings from newer schemas pass through verbatim
    /// (the PWA casts any string as CloseTxRole).
    #[test]
    fn unknown_tx_roles_are_preserved_verbatim() {
        let wire = serde_json::json!({
            "channelId": "ab",
            "txs": [{"txid": "t1", "role": "future_role"}],
            "createdAt": 1
        });
        let decoded = deserialize_close_record(&wire, 0).unwrap();
        assert_eq!(
            decoded.txs[0].role,
            CloseTxRole::Other("future_role".into())
        );
        assert_eq!(
            serialize_close_record(&decoded)["txs"][0]["role"],
            "future_role"
        );
    }

    // ---------- status derivation (close-record.test.ts:133-169) ----------

    /// The full PWA table, mapped onto U5's labels: completedAt → Complete /
    /// ResolvedUnverified; unconfirmed sweep → Returning; future claimable →
    /// WaitingTimelock (unknown tip too); past claimable, no sweep →
    /// Closing; confirmed sweep, not complete → Returning; bare → Closing.
    #[test]
    fn status_derivation_matches_the_pwa_table() {
        let sweep = |height: Option<u32>| CloseRecordTx {
            txid: "s".into(),
            role: CloseTxRole::Sweep,
            fee_sats: None,
            confirmed_at_height: height,
        };
        let cases: Vec<(CloseRecord, Option<u32>, CloseStatusLabel)> = vec![
            (
                record(|r| {
                    r.completed_at_ms = Some(1);
                    r.resolution = Some(Resolution::Verified);
                }),
                Some(100),
                CloseStatusLabel::Complete,
            ),
            (
                record(|r| {
                    r.completed_at_ms = Some(1);
                    r.resolution = Some(Resolution::Unverified);
                }),
                Some(100),
                CloseStatusLabel::ResolvedUnverified,
            ),
            (
                record(|r| r.txs = vec![sweep(None)]),
                Some(100),
                CloseStatusLabel::Returning,
            ),
            (
                record(|r| r.claimable_at_height = Some(200)),
                Some(100),
                CloseStatusLabel::WaitingTimelock,
            ),
            (
                record(|r| r.claimable_at_height = Some(200)),
                None,
                CloseStatusLabel::WaitingTimelock,
            ),
            (
                record(|r| r.claimable_at_height = Some(50)),
                Some(100),
                CloseStatusLabel::Closing,
            ),
            (
                record(|r| {
                    r.claimable_at_height = Some(50);
                    r.txs = vec![sweep(Some(90))];
                }),
                Some(100),
                CloseStatusLabel::Returning,
            ),
            (record(|_| {}), Some(100), CloseStatusLabel::Closing),
        ];
        for (record, tip, expected) in cases {
            assert_eq!(
                derive_close_status(&record, tip),
                expected,
                "record {record:?} at tip {tip:?}"
            );
        }
    }

    // ---------- store: local persistence + merge direction ----------

    fn store_in(dir: &std::path::Path) -> Arc<CloseRecordStore> {
        Arc::new(CloseRecordStore::new(
            Arc::new(FilesystemStore::new(dir.join("store"))),
            Arc::new(Logger),
        ))
    }

    #[test]
    fn upsert_persists_locally_and_survives_a_reload() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        store.upsert(record(|r| {
            r.close_type = CloseType::Force;
            r.expected_amount_sats = Some(70_000);
        }));

        let reloaded = store_in(dir.path());
        let row = reloaded.get("ab").expect("record survives a reload");
        assert_eq!(row.close_type, CloseType::Force);
        assert_eq!(row.expected_amount_sats, Some(70_000));
    }

    /// Direction is base = EXISTING (local), incoming = new facts: the
    /// stored closureReason survives, incoming measurements win.
    #[test]
    fn upsert_merges_with_the_existing_record_as_base() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        store.upsert(record(|r| {
            r.closure_reason = Some("first reason".into());
            r.expected_amount_sats = Some(5_000);
        }));
        store.upsert(record(|r| {
            r.closure_reason = Some("second reason".into());
            r.expected_amount_sats = Some(4_800);
        }));
        let row = store.get("ab").unwrap();
        assert_eq!(row.closure_reason.as_deref(), Some("first reason"));
        assert_eq!(row.expected_amount_sats, Some(4_800));
    }

    #[test]
    fn funding_txo_map_records_removes_and_survives_reload() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        let entry = FundingTxoEntry {
            txid: "f0".into(),
            vout: 1,
            timelock_blocks: Some(144),
        };
        store.record_funding_txo("chan1", entry.clone());
        store.record_funding_txo("chan1", entry.clone()); // idempotent
        assert_eq!(store.funding_txo_map().len(), 1);

        let reloaded = store_in(dir.path());
        assert_eq!(reloaded.funding_txo_map().get("chan1"), Some(&entry));
        reloaded.remove_funding_txo("chan1");
        assert!(reloaded.funding_txo_map().is_empty());
        assert!(store_in(dir.path()).funding_txo_map().is_empty());
    }

    /// U4 restore: the replaced wallet's records must not survive in memory.
    #[test]
    fn reset_drops_all_cached_state() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        store.upsert(record(|_| {}));
        store.record_funding_txo(
            "chan1",
            FundingTxoEntry {
                txid: "f".into(),
                vout: 0,
                timelock_blocks: None,
            },
        );
        store.set_last_tip_height(100);
        store.reset();
        assert!(store.get("ab").is_none());
        assert!(store.funding_txo_map().is_empty());
        assert!(store.last_tip_height().is_none());
    }

    // ---------- store: VSS singleton with field-wise merge on 409 ----------

    struct NullSink;
    impl EventSink for NullSink {
        fn emit(&self, _event: CoreEvent) {}
    }

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap()
    }

    fn vss_pair(
        dir: &std::path::Path,
        rt: &tokio::runtime::Runtime,
    ) -> (Arc<MockTransport>, Arc<VssBackedStore>) {
        let transport = Arc::new(MockTransport::new());
        let local = Arc::new(FilesystemStore::new(dir.join("store")));
        let vss = Arc::new(VssBackedStore::new(
            Some(Arc::clone(&transport) as Arc<dyn VssTransport>),
            local,
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
            BTreeSet::new(),
            false,
        ));
        (transport, vss)
    }

    fn wait_for(rt: &tokio::runtime::Runtime, mut check: impl FnMut() -> bool) {
        rt.block_on(async {
            for _ in 0..1_000 {
                if check() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
            panic!("condition never became true");
        });
    }

    /// R3/KTD-3: a 409 on `close_records` refetches the remote map, merges
    /// FIELD-WISE with base = local, rewrites, and folds the remote facts
    /// into the local store — never blob-LWW (which would clobber the other
    /// device's sweep txid with our commitment fee, or vice versa).
    #[test]
    fn vss_conflict_merges_field_wise_with_local_as_base() {
        let dir = tempfile::tempdir().unwrap();
        let rt = rt();
        let (transport, vss) = vss_pair(dir.path(), &rt);
        let store = store_in(dir.path());
        store.attach_vss(Arc::clone(&vss));

        // The other device knows the sweep txid and its own reason.
        let mut their_record = CloseRecord::skeleton("chan1", 2_000);
        their_record.closure_reason = Some("their reason".into());
        their_record.txs = vec![CloseRecordTx {
            txid: "sweep1".into(),
            role: CloseTxRole::Sweep,
            fee_sats: None,
            confirmed_at_height: Some(50),
        }];
        let their_map = serde_json::to_vec(&encode_records_map(
            &[("chan1".to_string(), their_record)].into_iter().collect(),
        ))
        .unwrap();
        transport.seed(CLOSE_RECORDS_VSS_KEY, &their_map, 4);

        // We know the commitment fee and our own reason; our cached version
        // is 0 → the put conflicts.
        let mut ours = CloseRecord::skeleton("chan1", 1_000);
        ours.closure_reason = Some("our reason".into());
        ours.txs = vec![CloseRecordTx {
            txid: "commit1".into(),
            role: CloseTxRole::Commitment,
            fee_sats: Some(2_000),
            confirmed_at_height: None,
        }];
        store.upsert(ours);

        wait_for(&rt, || {
            transport
                .value(CLOSE_RECORDS_VSS_KEY)
                .is_some_and(|(_, version)| version == 5)
        });
        let (bytes, _) = transport.value(CLOSE_RECORDS_VSS_KEY).unwrap();
        let merged = decode_records_map(&serde_json::from_slice(&bytes).unwrap(), 0);
        let merged_record = merged.get("chan1").expect("merged record on VSS");
        assert_eq!(
            merged_record.closure_reason.as_deref(),
            Some("our reason"),
            "base = LOCAL: our identity facts win"
        );
        assert_eq!(merged_record.txs.len(), 2, "both devices' txs survive");
        assert_eq!(merged_record.created_at_ms, 1_000, "min createdAt");

        // The remote facts were folded into the local store too.
        let local = store.get("chan1").unwrap();
        assert!(local.txs.iter().any(|tx| tx.txid == "sweep1"));
        assert!(local.txs.iter().any(|tx| tx.txid == "commit1"));
    }

    /// Init seeding: a remote snapshot merges into the local store (the
    /// cross-device restore path — PWA `initCloseRecords`).
    #[test]
    fn seed_from_remote_merges_into_the_local_store() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        store.upsert(record(|r| r.closure_reason = Some("local reason".into())));

        let mut remote_record = CloseRecord::skeleton("ab", 500);
        remote_record.expected_amount_sats = Some(9_000);
        let remote_map = serde_json::to_vec(&encode_records_map(
            &[("ab".to_string(), remote_record)].into_iter().collect(),
        ))
        .unwrap();
        store.seed_from_remote(&remote_map);

        let merged = store.get("ab").unwrap();
        assert_eq!(merged.closure_reason.as_deref(), Some("local reason"));
        assert_eq!(merged.expected_amount_sats, Some(9_000));
        assert_eq!(merged.created_at_ms, 500);
        // Durable: the merged state survives a reload.
        assert_eq!(
            store_in(dir.path()).get("ab").unwrap().expected_amount_sats,
            Some(9_000)
        );
    }

    // ---------- activity summaries (U5 seam) ----------

    /// KTD-7 absorption: EVERY attached txid (commitment + sweep) is
    /// reported so the on-chain arm skips them.
    #[test]
    fn summaries_absorb_all_attached_txids_and_derive_status() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        store.upsert(record(|r| {
            r.expected_amount_sats = Some(7_000);
            r.txs = vec![
                CloseRecordTx {
                    txid: "commit1".into(),
                    role: CloseTxRole::Commitment,
                    fee_sats: None,
                    confirmed_at_height: Some(10),
                },
                CloseRecordTx {
                    txid: "sweep1".into(),
                    role: CloseTxRole::Sweep,
                    fee_sats: None,
                    confirmed_at_height: None,
                },
            ];
        }));
        store.set_last_tip_height(100);

        let summaries = <CloseRecordStore as CloseRecordSource>::summaries(&store);
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].channel_id, "ab");
        assert_eq!(summaries[0].created_at_ms, 1_000);
        assert_eq!(summaries[0].expected_amount_sats, Some(7_000));
        assert_eq!(summaries[0].status, CloseStatusLabel::Returning);
        assert_eq!(summaries[0].absorbed_txids, vec!["commit1", "sweep1"]);
    }

    // ---------- signals (event-side, idempotent under replay) ----------

    #[test]
    fn channel_closed_signal_builds_the_record_from_map_and_classification() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        store.record_funding_txo(
            "chan1",
            FundingTxoEntry {
                txid: "f0".into(),
                vout: 0,
                timelock_blocks: Some(144),
            },
        );
        let classification = ClosureClassification {
            description: "Force closed by you".into(),
            close_type: CloseType::Force,
            initiator: Initiator::Local,
            has_onchain_tx: true,
        };
        on_channel_closed(&store, "chan1", &classification, None, Some(50_000), 1_000);
        // Replay: same event again — a no-op on stored facts.
        on_channel_closed(&store, "chan1", &classification, None, Some(50_000), 2_000);

        let record = store.get("chan1").unwrap();
        assert_eq!(record.close_type, CloseType::Force);
        assert_eq!(record.initiator, Initiator::Local);
        assert_eq!(
            record.closure_reason.as_deref(),
            Some("Force closed by you")
        );
        assert_eq!(
            record.funding_txo,
            Some(CloseOutpoint {
                txid: "f0".into(),
                vout: 0
            })
        );
        assert_eq!(record.timelock_blocks, Some(144), "map's to_self_delay");
        assert_eq!(record.expected_amount_sats, Some(50_000));
        assert_eq!(record.created_at_ms, 1_000, "replay keeps the first facts");
        assert!(
            store.funding_txo_map().is_empty(),
            "the record now carries the txo; the map entry is dropped"
        );
    }

    /// signals.ts:47-50 — no on-chain close tx and nothing to watch → no
    /// record, and the safety-net entry is dropped so reconciliation cannot
    /// resurrect it.
    #[test]
    fn channel_closed_without_onchain_tx_creates_no_record() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        store.record_funding_txo(
            "chan1",
            FundingTxoEntry {
                txid: "f0".into(),
                vout: 0,
                timelock_blocks: None,
            },
        );
        let classification = ClosureClassification {
            description: "Peer disconnected".into(),
            close_type: CloseType::Unknown,
            initiator: Initiator::Unknown,
            has_onchain_tx: false,
        };
        on_channel_closed(&store, "chan1", &classification, None, None, 1_000);
        assert!(store.get("chan1").is_none());
        assert!(store.funding_txo_map().is_empty());
    }

    #[test]
    fn commitment_broadcast_and_sweep_attribution_merge_idempotently() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        on_commitment_broadcast(&store, "chan1", "commit1", 2_500, 1_000);
        on_commitment_broadcast(&store, "chan1", "commit1", 2_500, 2_000); // replay

        let channels: HashSet<String> = ["chan1".to_string(), "chan2".to_string()]
            .into_iter()
            .collect();
        record_sweep_tx(&store, "sweep1", &channels, 3_000);
        record_sweep_tx(&store, "sweep1", &channels, 4_000); // replay

        let chan1 = store.get("chan1").unwrap();
        assert_eq!(chan1.close_type, CloseType::Force);
        assert_eq!(chan1.txs.len(), 2);
        let commit = chan1.txs.iter().find(|tx| tx.txid == "commit1").unwrap();
        assert_eq!(commit.role, CloseTxRole::Commitment);
        assert_eq!(commit.fee_sats, Some(2_500));
        assert!(chan1
            .txs
            .iter()
            .any(|tx| tx.txid == "sweep1" && tx.role == CloseTxRole::Sweep));
        // A batched sweep txid appears in every contributing channel's record.
        let chan2 = store.get("chan2").unwrap();
        assert_eq!(chan2.txs.len(), 1);
        assert_eq!(chan2.txs[0].txid, "sweep1");
    }

    // ---------- reconcile (chain truth) ----------

    #[derive(Default)]
    struct MockChain {
        tip: u32,
        /// (funding txid, vout) → spender txid.
        outspends: HashMap<(String, u32), String>,
        /// txid → confirmed height.
        heights: HashMap<String, u32>,
        fail_outspends: bool,
        queries: AtomicU32,
    }

    impl MockChain {
        fn budgeted_queries(&self) -> u32 {
            self.queries.load(Ordering::SeqCst)
        }
    }

    impl ChainTruth for MockChain {
        fn tip_height(&self) -> BoxFuture<'_, Result<u32, String>> {
            Box::pin(async move { Ok(self.tip) })
        }
        fn outspend<'a>(
            &'a self,
            txid: &'a str,
            vout: u32,
        ) -> BoxFuture<'a, Result<Option<String>, String>> {
            Box::pin(async move {
                self.queries.fetch_add(1, Ordering::SeqCst);
                if self.fail_outspends {
                    return Err("esplora 500".to_string());
                }
                Ok(self.outspends.get(&(txid.to_string(), vout)).cloned())
            })
        }
        fn tx_confirmed_height<'a>(
            &'a self,
            txid: &'a str,
        ) -> BoxFuture<'a, Result<Option<u32>, String>> {
            Box::pin(async move {
                self.queries.fetch_add(1, Ordering::SeqCst);
                Ok(self.heights.get(txid).copied())
            })
        }
    }

    #[derive(Default)]
    struct MockWallet(HashSet<String>);

    impl WalletReceipts for MockWallet {
        fn tx_confirmed_in_wallet(&self, txid: &str) -> bool {
            self.0.contains(txid)
        }
    }

    fn run_reconcile(
        store: &Arc<CloseRecordStore>,
        chain: &MockChain,
        wallet: &MockWallet,
        open: &HashSet<String>,
        pending_sweeps: &HashSet<String>,
        now_ms: u64,
    ) {
        rt().block_on(reconcile_close_records(
            store,
            chain,
            wallet,
            open,
            || pending_sweeps.clone(),
            now_ms,
            &Arc::new(Logger),
        ));
    }

    fn force_close_record(store: &Arc<CloseRecordStore>) {
        store.upsert(record(|r| {
            r.channel_id = "chan1".into();
            r.close_type = CloseType::Force;
            r.initiator = Initiator::Local;
            r.funding_txo = Some(CloseOutpoint {
                txid: "fund1".into(),
                vout: 0,
            });
            r.timelock_blocks = Some(144);
        }));
    }

    /// Steps (a)+(b2): the funding outspend discovers the confirmed close tx
    /// and derives the timelock expiry in the same pass.
    #[test]
    fn reconcile_discovers_the_funding_spend_and_derives_the_timelock() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        force_close_record(&store);

        let chain = MockChain {
            tip: 100,
            outspends: [(("fund1".to_string(), 0), "commit1".to_string())]
                .into_iter()
                .collect(),
            heights: [("commit1".to_string(), 98)].into_iter().collect(),
            ..Default::default()
        };
        run_reconcile(
            &store,
            &chain,
            &MockWallet::default(),
            &HashSet::new(),
            &HashSet::new(),
            5_000,
        );

        let record = store.get("chan1").unwrap();
        let commit = record.txs.iter().find(|tx| tx.txid == "commit1").unwrap();
        assert_eq!(
            commit.role,
            CloseTxRole::Commitment,
            "force close → commitment"
        );
        assert_eq!(commit.confirmed_at_height, Some(98));
        assert_eq!(
            record.claimable_at_height,
            Some(98 + 144),
            "claimable = close confirm height + timelock (b2)"
        );
        assert!(record.completed_at_ms.is_none(), "no completion yet");
    }

    /// SUPERSEDED COMMITMENT (the load-bearing case, reconcile.ts:174-179):
    /// a record holding only our own broadcast commitment keeps its funding
    /// outspend re-checked; the counterparty's confirmed commitment is
    /// discovered and recorded — ours can then never confirm.
    #[test]
    fn reconcile_discovers_a_superseding_counterparty_commitment() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        force_close_record(&store);
        // Our own broadcast-time claim, unconfirmed.
        on_commitment_broadcast(&store, "chan1", "ours1", 1_000, 1_500);

        let chain = MockChain {
            tip: 100,
            outspends: [(("fund1".to_string(), 0), "theirs1".to_string())]
                .into_iter()
                .collect(),
            heights: [("theirs1".to_string(), 99)].into_iter().collect(),
            ..Default::default()
        };
        run_reconcile(
            &store,
            &chain,
            &MockWallet::default(),
            &HashSet::new(),
            &HashSet::new(),
            5_000,
        );

        let record = store.get("chan1").unwrap();
        assert!(
            record
                .txs
                .iter()
                .any(|tx| tx.txid == "theirs1" && tx.confirmed_at_height == Some(99)),
            "the counterparty's confirmed commitment must be recorded: {record:?}"
        );
        assert!(
            record.txs.iter().any(|tx| tx.txid == "ours1"),
            "our broadcast claim stays as a fact"
        );
    }

    /// Mempool-window exception: with an UNCHANGED tip, a record with no
    /// confirmed close tx still gets its outspend checked; once a close tx
    /// confirmed, an unchanged tip costs zero queries.
    #[test]
    fn reconcile_mempool_window_rechecks_only_undiscovered_closes() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        force_close_record(&store);
        store.set_last_tip_height(100); // tip will NOT change

        let chain = MockChain {
            tip: 100,
            ..Default::default()
        };
        run_reconcile(
            &store,
            &chain,
            &MockWallet::default(),
            &HashSet::new(),
            &HashSet::new(),
            5_000,
        );
        assert_eq!(
            chain.budgeted_queries(),
            1,
            "the undiscovered record's outspend is checked every tick"
        );

        // Confirm the close: an unchanged tip now costs zero queries.
        store.upsert(record(|r| {
            r.channel_id = "chan1".into();
            r.txs = vec![CloseRecordTx {
                txid: "commit1".into(),
                role: CloseTxRole::Commitment,
                fee_sats: None,
                confirmed_at_height: Some(98),
            }];
        }));
        let quiet_chain = MockChain {
            tip: 100,
            ..Default::default()
        };
        run_reconcile(
            &store,
            &quiet_chain,
            &MockWallet::default(),
            &HashSet::new(),
            &HashSet::new(),
            5_000,
        );
        assert_eq!(quiet_chain.budgeted_queries(), 0);
    }

    /// The 8-query budget (reconcile.ts:52) bounds a pass no matter how many
    /// records are pending.
    #[test]
    fn reconcile_spends_at_most_eight_budgeted_queries_per_pass() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        for index in 0..12 {
            store.upsert(record(|r| {
                r.channel_id = format!("chan{index}");
                r.close_type = CloseType::Force;
                r.funding_txo = Some(CloseOutpoint {
                    txid: format!("fund{index}"),
                    vout: 0,
                });
            }));
        }
        let chain = MockChain {
            tip: 100,
            ..Default::default()
        };
        run_reconcile(
            &store,
            &chain,
            &MockWallet::default(),
            &HashSet::new(),
            &HashSet::new(),
            5_000,
        );
        assert!(
            chain.budgeted_queries() <= MAX_QUERIES_PER_PASS,
            "budget exceeded: {} queries",
            chain.budgeted_queries()
        );
    }

    /// Esplora errors leave records stale — they never read as "no spends"
    /// and never complete anything.
    #[test]
    fn reconcile_esplora_errors_never_complete_records() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        force_close_record(&store);
        let before = store.get("chan1").unwrap();

        let chain = MockChain {
            tip: 100,
            fail_outspends: true,
            ..Default::default()
        };
        run_reconcile(
            &store,
            &chain,
            &MockWallet::default(),
            &HashSet::new(),
            &HashSet::new(),
            5_000,
        );
        let after = store.get("chan1").unwrap();
        assert_eq!(after, before, "an erroring record must stay untouched");
    }

    /// Completion needs POSITIVE receipt evidence: a deeply-confirmed sweep
    /// visible in OUR wallet → completedAt + verified.
    #[test]
    fn reconcile_completes_verified_on_deep_wallet_receipt() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        store.upsert(record(|r| {
            r.channel_id = "chan1".into();
            r.close_type = CloseType::Force;
            r.txs = vec![
                CloseRecordTx {
                    txid: "commit1".into(),
                    role: CloseTxRole::Commitment,
                    fee_sats: None,
                    confirmed_at_height: Some(80),
                },
                CloseRecordTx {
                    txid: "sweep1".into(),
                    role: CloseTxRole::Sweep,
                    fee_sats: None,
                    confirmed_at_height: Some(90),
                },
            ];
        }));

        let chain = MockChain {
            tip: 100, // sweep at 90 → 11 confs ≥ 6
            ..Default::default()
        };
        let wallet = MockWallet(["sweep1".to_string()].into_iter().collect());
        run_reconcile(
            &store,
            &chain,
            &wallet,
            &HashSet::new(),
            &HashSet::new(),
            5_000,
        );

        let record = store.get("chan1").unwrap();
        assert_eq!(record.completed_at_ms, Some(5_000));
        assert_eq!(record.resolution, Some(Resolution::Verified));
        assert_eq!(
            derive_close_status(&record, Some(100)),
            CloseStatusLabel::Complete
        );
    }

    /// A close resolved on-chain whose funds our wallet never saw terminates
    /// as resolved_unverified — never laundered into "complete". Also: a
    /// pending un-swept output for the channel blocks completion.
    #[test]
    fn reconcile_resolves_unverified_after_the_timelock_without_receipt() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        store.upsert(record(|r| {
            r.channel_id = "chan1".into();
            r.close_type = CloseType::Force;
            r.expected_amount_sats = Some(40_000);
            r.claimable_at_height = Some(80);
            r.txs = vec![CloseRecordTx {
                txid: "commit1".into(),
                role: CloseTxRole::Commitment,
                fee_sats: None,
                confirmed_at_height: Some(60),
            }];
        }));

        let chain = MockChain {
            tip: 100, // claimable 80 + 6 ≤ 100, close 60 deeply confirmed
            ..Default::default()
        };
        // First: an un-swept pending output blocks completion.
        let pending: HashSet<String> = ["chan1".to_string()].into_iter().collect();
        run_reconcile(
            &store,
            &chain,
            &MockWallet::default(),
            &HashSet::new(),
            &pending,
            5_000,
        );
        assert!(store.get("chan1").unwrap().completed_at_ms.is_none());

        // Without the pending sweep it terminates as unverified.
        let chain = MockChain {
            tip: 101,
            ..Default::default()
        };
        run_reconcile(
            &store,
            &chain,
            &MockWallet::default(),
            &HashSet::new(),
            &HashSet::new(),
            6_000,
        );
        let record = store.get("chan1").unwrap();
        assert_eq!(record.completed_at_ms, Some(6_000));
        assert_eq!(record.resolution, Some(Resolution::Unverified));
        assert_eq!(
            derive_close_status(&record, Some(101)),
            CloseStatusLabel::ResolvedUnverified
        );
    }

    /// Safety-net records (reconcile.ts:125-151): a channel that vanished
    /// recordless (crash between ok() and the persist) is recreated from the
    /// funding-txo map; open channels and already-recorded closes are not.
    #[test]
    fn reconcile_creates_safety_net_records_for_vanished_channels() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        store.record_funding_txo(
            "gone",
            FundingTxoEntry {
                txid: "fundg".into(),
                vout: 1,
                timelock_blocks: Some(720),
            },
        );
        store.record_funding_txo(
            "open",
            FundingTxoEntry {
                txid: "fundo".into(),
                vout: 0,
                timelock_blocks: None,
            },
        );

        let chain = MockChain {
            tip: 100,
            ..Default::default()
        };
        let open: HashSet<String> = ["open".to_string()].into_iter().collect();
        run_reconcile(
            &store,
            &chain,
            &MockWallet::default(),
            &open,
            &HashSet::new(),
            5_000,
        );

        let record = store.get("gone").expect("safety-net record created");
        assert_eq!(record.closure_reason.as_deref(), Some(OFFLINE_CLOSE_REASON));
        assert_eq!(record.close_type, CloseType::Unknown);
        assert_eq!(record.timelock_blocks, Some(720));
        assert_eq!(
            record.funding_txo,
            Some(CloseOutpoint {
                txid: "fundg".into(),
                vout: 1
            })
        );
        assert!(store.get("open").is_none(), "open channels stay untouched");
        assert!(store.funding_txo_map().contains_key("open"));
        assert!(!store.funding_txo_map().contains_key("gone"));
    }

    /// b2's scope: coop closes and REMOTE-initiated force closes derive no
    /// timelock (to_self_delay encumbers only the broadcaster's to_local).
    #[test]
    fn reconcile_derives_no_timelock_for_remote_initiated_closes() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        store.upsert(record(|r| {
            r.channel_id = "chan1".into();
            r.close_type = CloseType::Force;
            r.initiator = Initiator::Remote;
            r.timelock_blocks = Some(144);
            r.txs = vec![CloseRecordTx {
                txid: "commit1".into(),
                role: CloseTxRole::Commitment,
                fee_sats: None,
                confirmed_at_height: Some(90),
            }];
        }));

        let chain = MockChain {
            tip: 100,
            ..Default::default()
        };
        run_reconcile(
            &store,
            &chain,
            &MockWallet::default(),
            &HashSet::new(),
            &HashSet::new(),
            5_000,
        );
        assert_eq!(store.get("chan1").unwrap().claimable_at_height, None);
    }
}
