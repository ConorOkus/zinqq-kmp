//! Persisted payment history and the unified activity feed (U5: R11 store
//! half, KTD-7; R14 — the merge lives in core, shells never merge).
//!
//! [`PersistedPayment`] rows mirror the PWA's `PersistedPayment`
//! (`zinq/src/ldk/storage/payment-history.ts`): keyed by payment id hex (the
//! payment hash for inbound and bolt11-outbound rows; a random id for BOLT12
//! sends), serialized with camelCase keys and amounts as strings — the PWA
//! stores JS bigints as strings, so the blob shapes stay interchangeable.
//!
//! Write discipline (KTD-5/U5):
//! - a PENDING row is written at dispatch (`record_pending`);
//! - settles come from LDK events (`PaymentSent`/`PaymentFailed` via
//!   [`PaymentStore::settle`], `PaymentClaimed` via
//!   [`PaymentStore::record_claimed`]) and are persisted BEFORE the causing
//!   event is considered handled (persist-then-ack, `node::handle_ldk_event`);
//! - every settle is idempotent under event replay: settling a settled row
//!   and re-claiming a claimed payment are no-ops.
//!
//! [`merge_activity`] implements KTD-7's merge rules from the PWA's
//! `zinq/src/hooks/use-transaction-history.ts`: failed Lightning rows hidden,
//! on-chain transactions as net sent/received with close-absorbed txids
//! skipped, one row per close record with a STABLE timestamp, all sorted
//! descending by time. U5 defines only the close-record READ interface
//! ([`CloseRecordSource`], shaped after the PWA's
//! `zinq/src/ldk/close-records/close-record.ts`); U10 implements the store.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use lightning::log_error;
use lightning::util::logger::Logger as _;
use lightning::util::persist::KVStoreSync;
use lightning_persister::fs_store::FilesystemStore;
use serde::{Deserialize, Serialize};

use crate::types::Logger;

/// KVStore namespace holding one JSON row per payment, keyed by payment id
/// hex. Local-only by decision (the PWA keeps payment history out of VSS).
pub(crate) const PAYMENT_HISTORY_PRIMARY_NAMESPACE: &str = "payment_history";
pub(crate) const PAYMENT_HISTORY_SECONDARY_NAMESPACE: &str = "";

/// Startup-reconcile grace: a pending row with no LDK counterpart younger
/// than this is left alone (its dispatch may still be settling in).
pub(crate) const RECONCILE_GRACE_MS: u64 = 60_000;

/// The failure reason a startup reconcile writes onto an orphaned pending row
/// (a dispatch interrupted by process death before LDK registered it).
pub const INTERRUPTED_REASON: &str = "interrupted";

/// Payment direction, serialized as the PWA's lowercase literals.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, uniffi::Enum)]
#[serde(rename_all = "lowercase")]
pub enum PaymentDirection {
    Inbound,
    Outbound,
}

/// Payment status, serialized as the PWA's lowercase literals.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, uniffi::Enum)]
#[serde(rename_all = "lowercase")]
pub enum PaymentStatus {
    Pending,
    Succeeded,
    Failed,
}

/// One persisted payment row — the PWA's `PersistedPayment` shape
/// (`payment-history.ts:5-23`): camelCase keys, `amountMsat`/`feePaidMsat`
/// serialized as strings (bigint-safe), `createdAt` in unix ms.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, uniffi::Record)]
#[serde(rename_all = "camelCase")]
pub struct PersistedPayment {
    /// Payment id hex — equals the payment hash for inbound and
    /// bolt11-outbound rows; BOLT12 sends use a random id (PWA parity).
    pub payment_hash: String,
    pub direction: PaymentDirection,
    #[serde(with = "u64_as_string")]
    pub amount_msat: u64,
    pub status: PaymentStatus,
    #[serde(with = "opt_u64_as_string")]
    pub fee_paid_msat: Option<u64>,
    /// Unix milliseconds, the PWA's `createdAt` (`Date.now()`).
    #[serde(rename = "createdAt")]
    pub created_at_ms: u64,
    pub failure_reason: Option<String>,
}

/// `u64` <-> JSON string (the PWA serializes bigints as strings).
mod u64_as_string {
    use serde::{Deserialize as _, Deserializer, Serializer};

    pub(super) fn serialize<S: Serializer>(value: &u64, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&value.to_string())
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
        let raw = String::deserialize(deserializer)?;
        raw.parse().map_err(serde::de::Error::custom)
    }
}

/// `Option<u64>` <-> JSON string-or-null (the PWA's `feePaidMsat`).
mod opt_u64_as_string {
    use serde::{Deserialize as _, Deserializer, Serializer};

    pub(super) fn serialize<S: Serializer>(
        value: &Option<u64>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        match value {
            Some(value) => serializer.serialize_some(&value.to_string()),
            None => serializer.serialize_none(),
        }
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<u64>, D::Error> {
        let raw: Option<String> = Option::deserialize(deserializer)?;
        raw.map(|value| value.parse().map_err(serde::de::Error::custom))
            .transpose()
    }
}

/// Typed history-store failures. A failed persist means the settle is NOT
/// durable — callers on the LDK event path must replay the event rather than
/// ack it (persist-then-ack).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistoryError {
    /// The row failed to serialize or write to the KVStore.
    Persist { detail: String },
}

impl std::fmt::Display for HistoryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HistoryError::Persist { detail } => {
                write!(f, "failed to persist a payment history row: {detail}")
            }
        }
    }
}

impl std::error::Error for HistoryError {}

/// The persisted payment store. Like the event queue, it owns its own
/// `FilesystemStore` handle over the shared store directory, so rows are
/// readable and writable while the node is stopped.
pub(crate) struct PaymentStore {
    rows: Mutex<HashMap<String, PersistedPayment>>,
    kv_store: Arc<FilesystemStore>,
    logger: Arc<Logger>,
}

impl PaymentStore {
    /// Loads all persisted rows. A corrupt row is logged and skipped
    /// (degrade, don't brick — history drives UI, not funds).
    pub(crate) fn new(kv_store: Arc<FilesystemStore>, logger: Arc<Logger>) -> Self {
        let mut rows = HashMap::new();
        let keys = match kv_store.list(
            PAYMENT_HISTORY_PRIMARY_NAMESPACE,
            PAYMENT_HISTORY_SECONDARY_NAMESPACE,
        ) {
            Ok(keys) => keys,
            Err(e) if e.kind() == lightning::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => {
                log_error!(
                    logger,
                    "Failed to list payment history, starting empty: {e}"
                );
                Vec::new()
            }
        };
        for key in keys {
            let bytes = match kv_store.read(
                PAYMENT_HISTORY_PRIMARY_NAMESPACE,
                PAYMENT_HISTORY_SECONDARY_NAMESPACE,
                &key,
            ) {
                Ok(bytes) => bytes,
                Err(e) => {
                    log_error!(logger, "Failed to read payment row {key}, skipping: {e}");
                    continue;
                }
            };
            match serde_json::from_slice::<PersistedPayment>(&bytes) {
                Ok(row) => {
                    rows.insert(key, row);
                }
                Err(e) => {
                    log_error!(logger, "Corrupt payment row {key}, skipping: {e}");
                }
            }
        }
        Self {
            rows: Mutex::new(rows),
            kv_store,
            logger,
        }
    }

    /// Serializes and writes one row under its payment id. Called under the
    /// rows lock, BEFORE the in-memory map is updated — memory never runs
    /// ahead of disk, so a failed persist leaves the row replayable.
    fn persist_row(&self, row: &PersistedPayment) -> Result<(), HistoryError> {
        let bytes = serde_json::to_vec(row).map_err(|e| HistoryError::Persist {
            detail: format!("serialize: {e}"),
        })?;
        self.kv_store
            .write(
                PAYMENT_HISTORY_PRIMARY_NAMESPACE,
                PAYMENT_HISTORY_SECONDARY_NAMESPACE,
                &row.payment_hash,
                bytes,
            )
            .map_err(|e| {
                log_error!(
                    self.logger,
                    "Failed to persist payment row {}: {e}",
                    row.payment_hash
                );
                HistoryError::Persist {
                    detail: e.to_string(),
                }
            })
    }

    /// Dispatch writer: a PENDING row keyed by `payment_id` (PWA
    /// `context.tsx:1011`/`1069`). Idempotent against the in-flight and paid
    /// cases: an existing PENDING or SUCCEEDED row is left untouched (the
    /// original attempt owns the outcome). A FAILED row is re-armed to a
    /// fresh PENDING row: re-dispatching a previously failed payment is a
    /// genuinely new attempt.
    pub(crate) fn record_pending(
        &self,
        payment_id: &str,
        direction: PaymentDirection,
        amount_msat: u64,
        created_at_ms: u64,
    ) -> Result<(), HistoryError> {
        let mut rows = self.rows.lock().unwrap();
        if let Some(existing) = rows.get(payment_id) {
            if existing.status != PaymentStatus::Failed {
                return Ok(());
            }
        }
        let row = PersistedPayment {
            payment_hash: payment_id.to_string(),
            direction,
            amount_msat,
            status: PaymentStatus::Pending,
            fee_paid_msat: None,
            created_at_ms,
            failure_reason: None,
        };
        self.persist_row(&row)?;
        rows.insert(payment_id.to_string(), row);
        Ok(())
    }

    /// Event settler (PWA `updatePaymentStatus`, `payment-history.ts:38-53`):
    /// moves a PENDING row to `status`, filling fee/reason. No-ops when the
    /// row is missing or already settled — idempotent under event replay.
    /// The row is durable on disk before this returns `Ok`.
    pub(crate) fn settle(
        &self,
        payment_id: &str,
        status: PaymentStatus,
        fee_paid_msat: Option<u64>,
        failure_reason: Option<String>,
    ) -> Result<(), HistoryError> {
        debug_assert_ne!(
            status,
            PaymentStatus::Pending,
            "settle target must be terminal"
        );
        let mut rows = self.rows.lock().unwrap();
        let Some(existing) = rows.get(payment_id) else {
            return Ok(()); // PWA parity: nothing stored, nothing to settle.
        };
        if existing.status != PaymentStatus::Pending {
            return Ok(()); // Already settled: replayed events are no-ops.
        }
        let mut row = existing.clone();
        row.status = status;
        row.fee_paid_msat = fee_paid_msat.or(row.fee_paid_msat);
        row.failure_reason = failure_reason.or(row.failure_reason);
        self.persist_row(&row)?;
        rows.insert(payment_id.to_string(), row);
        Ok(())
    }

    /// Claim writer (PWA `event-handler.ts:281-289`): an inbound SUCCEEDED
    /// row created at claim time, fee `None`. A replayed claim for an
    /// existing row is a no-op — re-claiming never duplicates.
    pub(crate) fn record_claimed(
        &self,
        payment_hash: &str,
        amount_msat: u64,
        created_at_ms: u64,
    ) -> Result<(), HistoryError> {
        let mut rows = self.rows.lock().unwrap();
        if rows.contains_key(payment_hash) {
            return Ok(()); // Replayed claim: the first claim's facts stand.
        }
        let row = PersistedPayment {
            payment_hash: payment_hash.to_string(),
            direction: PaymentDirection::Inbound,
            amount_msat,
            status: PaymentStatus::Succeeded,
            fee_paid_msat: None,
            created_at_ms,
            failure_reason: None,
        };
        self.persist_row(&row)?;
        rows.insert(payment_hash.to_string(), row);
        Ok(())
    }

    /// Startup reconcile (U5): every PENDING row with no live LDK
    /// counterpart (`live_payment_ids`) older than [`RECONCILE_GRACE_MS`] is
    /// settled FAILED with the [`INTERRUPTED_REASON`] — no permanent phantom
    /// in-flight rows. Returns how many rows were interrupted.
    pub(crate) fn reconcile_pending(
        &self,
        live_payment_ids: &HashSet<String>,
        now_ms: u64,
    ) -> Result<usize, HistoryError> {
        let stale: Vec<String> = {
            let rows = self.rows.lock().unwrap();
            rows.values()
                .filter(|row| {
                    row.status == PaymentStatus::Pending
                        && !live_payment_ids.contains(&row.payment_hash)
                        && now_ms.saturating_sub(row.created_at_ms) > RECONCILE_GRACE_MS
                })
                .map(|row| row.payment_hash.clone())
                .collect()
        };
        for payment_id in &stale {
            self.settle(
                payment_id,
                PaymentStatus::Failed,
                None,
                Some(INTERRUPTED_REASON.to_string()),
            )?;
        }
        Ok(stale.len())
    }

    /// A row by payment id, if present.
    pub(crate) fn get(&self, payment_id: &str) -> Option<PersistedPayment> {
        self.rows.lock().unwrap().get(payment_id).cloned()
    }

    /// All rows, in no particular order.
    pub(crate) fn rows(&self) -> Vec<PersistedPayment> {
        self.rows.lock().unwrap().values().cloned().collect()
    }
}

// ---------------------------------------------------------------------------
// Close-record read interface (U5 defines the shape; U10 implements the store)
// ---------------------------------------------------------------------------

/// Derived close display status — mirrors the PWA's `CloseStatus`
/// (`close-record.ts:56-61`). U10's store derives it; U5 only consumes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum CloseStatusLabel {
    Closing,
    WaitingTimelock,
    Returning,
    Complete,
    ResolvedUnverified,
}

/// The close-record fields the activity merge needs (`close-record.ts:26-51`
/// projected down): identity, the STABLE `createdAt` sort key, the estimated
/// return amount (None while unknown — render "—", never a lying 0), the
/// derived status, and the txids the close absorbs from the on-chain list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloseRecordSummary {
    pub channel_id: String,
    /// Stable history sort key, set at event time (`close-record.ts:44`).
    pub created_at_ms: u64,
    /// LDK's last-known local balance at close — an estimate, never measured.
    pub expected_amount_sats: Option<u64>,
    pub status: CloseStatusLabel,
    /// Txids of this close's transactions (commitment/sweep/closing/...);
    /// the merge skips these in the on-chain arm (absorption).
    pub absorbed_txids: Vec<String>,
}

/// Read seam for close records. U10's real store implements this;
/// [`NoCloseRecords`] serves until then.
pub(crate) trait CloseRecordSource: Send + Sync {
    fn summaries(&self) -> Vec<CloseRecordSummary>;
}

/// Default empty source used until U10 lands the close-record store.
pub(crate) struct NoCloseRecords;

impl CloseRecordSource for NoCloseRecords {
    fn summaries(&self) -> Vec<CloseRecordSummary> {
        Vec::new()
    }
}

// ---------------------------------------------------------------------------
// Unified activity (KTD-7)
// ---------------------------------------------------------------------------

/// An on-chain transaction as the merge consumes it (from the bdk wallet).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OnchainTxSummary {
    pub txid: String,
    /// Sum of this wallet's inputs spent by the tx, in sats.
    pub sent_sats: u64,
    /// Sum of this wallet's outputs created by the tx, in sats.
    pub received_sats: u64,
    pub confirmed: bool,
    /// Confirmation block time, unix seconds.
    pub confirmation_time_secs: Option<u64>,
    /// First seen in the mempool, unix seconds.
    pub first_seen_secs: Option<u64>,
}

/// Which layer an activity row comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum ActivityKind {
    Lightning,
    Onchain,
    ChannelClose,
}

/// Row direction (the PWA's `'sent' | 'received'`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum ActivityDirection {
    Sent,
    Received,
}

/// Row status (the PWA's `'confirmed' | 'pending' | 'failed'`). The feed
/// never emits `Failed` (failed Lightning rows are hidden, KTD-7); the
/// variant exists so the FFI shape is stable for detail surfaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum ActivityStatus {
    Pending,
    Confirmed,
    Failed,
}

/// One unified activity row (the PWA's `UnifiedTransaction`,
/// `use-transaction-history.ts:8-24`). Amounts are RAW — msat for Lightning,
/// sats for on-chain and close rows; flooring msat to sats for display is
/// shell work (R14 keeps only rendering there).
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ActivityRow {
    /// Payment id hex, txid, or `close:{channelId}` — stable per source row.
    pub id: String,
    pub kind: ActivityKind,
    pub direction: Option<ActivityDirection>,
    /// Raw msat, Lightning rows only.
    pub amount_msat: Option<u64>,
    /// Raw sats: on-chain net amount, or a close's expected return (`None`
    /// while unknown — render "—", never a lying "0 sats").
    pub amount_sats: Option<u64>,
    pub status: ActivityStatus,
    /// Unix ms sort key (stable for close rows).
    pub created_at_ms: u64,
    pub payment_hash: Option<String>,
    pub txid: Option<String>,
    pub channel_id: Option<String>,
    pub close_status: Option<CloseStatusLabel>,
    pub failure_reason: Option<String>,
}

/// KTD-7 merge, mirroring `use-transaction-history.ts:43-108`:
/// 1. absorbed txids come from the SAME close snapshot that emits close rows;
/// 2. on-chain txs render as net sent/received, absorbed txids skipped,
///    timestamp = confirmation time, else first seen, else 0;
/// 3. Lightning rows skip FAILED; pending stays pending, else confirmed;
/// 4. one row per close record: id `close:{channelId}`, direction received,
///    amount = expected sats or None, STABLE `createdAt`, confirmed only for
///    Complete/ResolvedUnverified;
/// 5. descending sort by timestamp (stable, preserving the push order for
///    equal keys).
pub(crate) fn merge_activity(
    payments: &[PersistedPayment],
    onchain_txs: &[OnchainTxSummary],
    close_records: &[CloseRecordSummary],
) -> Vec<ActivityRow> {
    let mut rows = Vec::new();

    // Absorption set from the SAME close snapshot that emits the close rows
    // (use-transaction-history.ts:50 — two snapshots would double-display).
    let absorbed: HashSet<&str> = close_records
        .iter()
        .flat_map(|record| record.absorbed_txids.iter().map(String::as_str))
        .collect();

    for tx in onchain_txs {
        if absorbed.contains(tx.txid.as_str()) {
            continue;
        }
        let is_send = tx.sent_sats > tx.received_sats;
        let net_sats = tx.sent_sats.abs_diff(tx.received_sats);
        let created_at_ms = tx
            .confirmation_time_secs
            .or(tx.first_seen_secs)
            .map(|secs| secs * 1_000)
            .unwrap_or(0);
        rows.push(ActivityRow {
            id: tx.txid.clone(),
            kind: ActivityKind::Onchain,
            direction: Some(if is_send {
                ActivityDirection::Sent
            } else {
                ActivityDirection::Received
            }),
            amount_msat: None,
            amount_sats: Some(net_sats),
            status: if tx.confirmed {
                ActivityStatus::Confirmed
            } else {
                ActivityStatus::Pending
            },
            created_at_ms,
            payment_hash: None,
            txid: Some(tx.txid.clone()),
            channel_id: None,
            close_status: None,
            failure_reason: None,
        });
    }

    for payment in payments {
        if payment.status == PaymentStatus::Failed {
            continue; // KTD-7: failed Lightning rows are hidden.
        }
        rows.push(ActivityRow {
            id: payment.payment_hash.clone(),
            kind: ActivityKind::Lightning,
            direction: Some(match payment.direction {
                PaymentDirection::Outbound => ActivityDirection::Sent,
                PaymentDirection::Inbound => ActivityDirection::Received,
            }),
            amount_msat: Some(payment.amount_msat),
            amount_sats: None,
            status: if payment.status == PaymentStatus::Pending {
                ActivityStatus::Pending
            } else {
                ActivityStatus::Confirmed
            },
            created_at_ms: payment.created_at_ms,
            payment_hash: Some(payment.payment_hash.clone()),
            txid: None,
            channel_id: None,
            close_status: None,
            failure_reason: None,
        });
    }

    for record in close_records {
        rows.push(ActivityRow {
            id: format!("close:{}", record.channel_id),
            kind: ActivityKind::ChannelClose,
            direction: Some(ActivityDirection::Received),
            amount_msat: None,
            amount_sats: record.expected_amount_sats,
            status: match record.status {
                CloseStatusLabel::Complete | CloseStatusLabel::ResolvedUnverified => {
                    ActivityStatus::Confirmed
                }
                _ => ActivityStatus::Pending,
            },
            // Stable sort key — rows must not hop as facts arrive.
            created_at_ms: record.created_at_ms,
            payment_hash: None,
            txid: None,
            channel_id: Some(record.channel_id.clone()),
            close_status: Some(record.status),
            failure_reason: None,
        });
    }

    // Stable descending sort: equal timestamps keep the PWA's push order
    // (on-chain, lightning, closes).
    rows.sort_by(|a, b| b.created_at_ms.cmp(&a.created_at_ms));
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn store_in(dir: &Path) -> PaymentStore {
        PaymentStore::new(
            Arc::new(FilesystemStore::new(dir.join("store"))),
            Arc::new(Logger),
        )
    }

    fn raw_row_bytes(dir: &Path, key: &str) -> Vec<u8> {
        FilesystemStore::new(dir.join("store"))
            .read(
                PAYMENT_HISTORY_PRIMARY_NAMESPACE,
                PAYMENT_HISTORY_SECONDARY_NAMESPACE,
                key,
            )
            .expect("row must be persisted")
    }

    const HASH_A: &str = "1111111111111111111111111111111111111111111111111111111111111111";
    const HASH_B: &str = "2222222222222222222222222222222222222222222222222222222222222222";

    // ---------- persistence & PWA blob shape ----------

    /// R11: rows survive a store rebuild, and the persisted blob has the
    /// PWA's exact shape — camelCase keys, amounts as STRINGS, fee null.
    #[test]
    fn pending_row_round_trips_and_matches_the_pwa_blob_shape() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        store
            .record_pending(
                HASH_A,
                PaymentDirection::Outbound,
                25_000,
                1_753_000_000_000,
            )
            .unwrap();

        let blob: serde_json::Value = serde_json::from_slice(&raw_row_bytes(dir.path(), HASH_A))
            .expect("persisted row must be JSON");
        assert_eq!(blob["paymentHash"], HASH_A);
        assert_eq!(blob["direction"], "outbound");
        assert_eq!(blob["amountMsat"], "25000", "bigint-as-string parity");
        assert_eq!(blob["status"], "pending");
        assert_eq!(blob["feePaidMsat"], serde_json::Value::Null);
        assert_eq!(blob["createdAt"], 1_753_000_000_000u64);
        assert_eq!(blob["failureReason"], serde_json::Value::Null);

        drop(store);
        let reloaded = store_in(dir.path());
        let row = reloaded.get(HASH_A).expect("row must survive a rebuild");
        assert_eq!(row.amount_msat, 25_000);
        assert_eq!(row.status, PaymentStatus::Pending);
        assert_eq!(row.direction, PaymentDirection::Outbound);
        assert_eq!(row.created_at_ms, 1_753_000_000_000);
    }

    /// A blob written in the PWA's serialized shape parses into a row
    /// (string amounts round-trip through u64).
    #[test]
    fn pwa_shaped_blob_parses_including_string_fee() {
        let dir = tempfile::tempdir().unwrap();
        let blob = format!(
            r#"{{"paymentHash":"{HASH_B}","direction":"inbound","amountMsat":"123456789012345",
                "status":"succeeded","feePaidMsat":"2100","createdAt":1753000000123,
                "failureReason":null}}"#
        );
        FilesystemStore::new(dir.path().join("store"))
            .write(
                PAYMENT_HISTORY_PRIMARY_NAMESPACE,
                PAYMENT_HISTORY_SECONDARY_NAMESPACE,
                HASH_B,
                blob.into_bytes(),
            )
            .unwrap();

        let store = store_in(dir.path());
        let row = store.get(HASH_B).expect("PWA-shaped blob must load");
        assert_eq!(row.amount_msat, 123_456_789_012_345);
        assert_eq!(row.fee_paid_msat, Some(2_100));
        assert_eq!(row.status, PaymentStatus::Succeeded);
        assert_eq!(row.direction, PaymentDirection::Inbound);
    }

    /// A corrupt persisted row degrades (skipped + logged), never bricks.
    #[test]
    fn corrupt_row_is_skipped_and_the_store_keeps_working() {
        let dir = tempfile::tempdir().unwrap();
        FilesystemStore::new(dir.path().join("store"))
            .write(
                PAYMENT_HISTORY_PRIMARY_NAMESPACE,
                PAYMENT_HISTORY_SECONDARY_NAMESPACE,
                HASH_A,
                b"not json".to_vec(),
            )
            .unwrap();

        let store = store_in(dir.path());
        assert!(store.rows().is_empty(), "corrupt row must be skipped");
        store
            .record_pending(HASH_B, PaymentDirection::Outbound, 1_000, 1)
            .unwrap();
        assert!(store.get(HASH_B).is_some());
    }

    // ---------- settle idempotency (exactly-once under replay) ----------

    /// Dispatch→settle settles exactly once: a replayed PaymentSent-style
    /// settle is a no-op and cannot overwrite the recorded fee; the settle is
    /// durable (visible after a rebuild) — persist-then-ack's precondition.
    #[test]
    fn settle_is_idempotent_and_durable_across_rebuilds() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        store
            .record_pending(HASH_A, PaymentDirection::Outbound, 50_000, 10)
            .unwrap();

        store
            .settle(HASH_A, PaymentStatus::Succeeded, Some(12), None)
            .unwrap();
        // Replay (crash between settle-persist and event ack): a no-op.
        store
            .settle(HASH_A, PaymentStatus::Succeeded, Some(99), None)
            .unwrap();
        let row = store.get(HASH_A).unwrap();
        assert_eq!(row.status, PaymentStatus::Succeeded);
        assert_eq!(
            row.fee_paid_msat,
            Some(12),
            "replay must not overwrite the fee"
        );

        // The settle was persisted BEFORE returning Ok: a rebuilt store (the
        // crash-before-ack process) sees it and its own replay is a no-op too.
        drop(store);
        let rebuilt = store_in(dir.path());
        assert_eq!(
            rebuilt.get(HASH_A).unwrap().status,
            PaymentStatus::Succeeded
        );
        rebuilt
            .settle(HASH_A, PaymentStatus::Failed, None, Some("late".into()))
            .unwrap();
        let row = rebuilt.get(HASH_A).unwrap();
        assert_eq!(
            row.status,
            PaymentStatus::Succeeded,
            "settled rows never regress"
        );
        assert_eq!(row.failure_reason, None);
    }

    /// Settling a missing row is a no-op (PWA `updatePaymentStatus` returns
    /// when nothing is stored, `payment-history.ts:44-45`).
    #[test]
    fn settling_a_missing_row_is_a_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        store
            .settle(HASH_A, PaymentStatus::Failed, None, Some("no route".into()))
            .unwrap();
        assert!(store.get(HASH_A).is_none());
        assert!(store.rows().is_empty());
    }

    /// Re-claiming (a replayed PaymentClaimed) never duplicates the inbound
    /// row, and keeps the FIRST claim's facts.
    #[test]
    fn record_claimed_is_idempotent_under_replay() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        store.record_claimed(HASH_A, 250_000, 1_000).unwrap();
        store.record_claimed(HASH_A, 999_999, 2_000).unwrap();

        let rows = store.rows();
        assert_eq!(rows.len(), 1, "re-claim must not duplicate");
        let row = store.get(HASH_A).unwrap();
        assert_eq!(row.direction, PaymentDirection::Inbound);
        assert_eq!(row.status, PaymentStatus::Succeeded);
        assert_eq!(row.amount_msat, 250_000);
        assert_eq!(row.created_at_ms, 1_000, "first claim's facts win");
        assert_eq!(
            row.fee_paid_msat, None,
            "inbound rows carry no fee (PWA parity)"
        );
    }

    /// Dispatch idempotency: an in-flight (pending) or paid (succeeded) row
    /// is never clobbered by a duplicate dispatch, but a FAILED row is
    /// re-armed — re-paying a failed invoice is a new attempt.
    #[test]
    fn record_pending_never_clobbers_in_flight_or_paid_rows_but_rearms_failed() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        store
            .record_pending(HASH_A, PaymentDirection::Outbound, 100, 1)
            .unwrap();
        store
            .record_pending(HASH_A, PaymentDirection::Outbound, 100, 2)
            .unwrap();
        assert_eq!(
            store.get(HASH_A).unwrap().created_at_ms,
            1,
            "pending: no clobber"
        );

        store
            .settle(HASH_A, PaymentStatus::Succeeded, None, None)
            .unwrap();
        store
            .record_pending(HASH_A, PaymentDirection::Outbound, 100, 3)
            .unwrap();
        assert_eq!(
            store.get(HASH_A).unwrap().status,
            PaymentStatus::Succeeded,
            "succeeded: no clobber"
        );

        store
            .record_pending(HASH_B, PaymentDirection::Outbound, 200, 4)
            .unwrap();
        store
            .settle(HASH_B, PaymentStatus::Failed, None, Some("no route".into()))
            .unwrap();
        store
            .record_pending(HASH_B, PaymentDirection::Outbound, 200, 5)
            .unwrap();
        let rearmed = store.get(HASH_B).unwrap();
        assert_eq!(rearmed.status, PaymentStatus::Pending, "failed rows re-arm");
        assert_eq!(rearmed.created_at_ms, 5, "a re-dispatch is a fresh attempt");
        assert_eq!(rearmed.failure_reason, None);
    }

    /// The persist-then-ack contract's failure half: when the settle CANNOT
    /// be made durable, `settle` errors (so the LDK event is replayed, not
    /// acked) and the in-memory row stays PENDING — memory never runs ahead
    /// of disk.
    #[cfg(unix)]
    #[test]
    fn settle_persist_failure_errors_and_leaves_the_row_pending() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        store
            .record_pending(HASH_A, PaymentDirection::Outbound, 100, 1)
            .unwrap();

        let namespace_dir = dir
            .path()
            .join("store")
            .join(PAYMENT_HISTORY_PRIMARY_NAMESPACE);
        let writable = std::fs::metadata(&namespace_dir).unwrap().permissions();
        std::fs::set_permissions(&namespace_dir, std::fs::Permissions::from_mode(0o555)).unwrap();

        let result = store.settle(HASH_A, PaymentStatus::Succeeded, Some(7), None);
        std::fs::set_permissions(&namespace_dir, writable).unwrap();

        assert!(
            matches!(result, Err(HistoryError::Persist { .. })),
            "a non-durable settle must surface as an error, got {result:?}"
        );
        assert_eq!(
            store.get(HASH_A).unwrap().status,
            PaymentStatus::Pending,
            "memory must not run ahead of disk"
        );

        // The replayed event settles cleanly once persistence recovers.
        store
            .settle(HASH_A, PaymentStatus::Succeeded, Some(7), None)
            .unwrap();
        assert_eq!(store.get(HASH_A).unwrap().status, PaymentStatus::Succeeded);
    }

    // ---------- startup reconcile ----------

    /// Orphaned pending rows past the grace are failed as "interrupted";
    /// rows with a live LDK counterpart, young orphans, and settled rows are
    /// untouched.
    #[test]
    fn reconcile_interrupts_only_stale_orphaned_pending_rows() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        let now_ms = 10 * RECONCILE_GRACE_MS;

        // Stale orphan: no LDK counterpart, older than the grace.
        store
            .record_pending(HASH_A, PaymentDirection::Outbound, 1, 1_000)
            .unwrap();
        // Live in LDK (list_recent_payments knows it): stays pending forever.
        store
            .record_pending(HASH_B, PaymentDirection::Outbound, 2, 1_000)
            .unwrap();
        // Young orphan: dispatched moments ago, still registering.
        let young = "3333333333333333333333333333333333333333333333333333333333333333";
        store
            .record_pending(young, PaymentDirection::Outbound, 3, now_ms - 1_000)
            .unwrap();
        // Already settled: reconcile never touches it.
        let done = "4444444444444444444444444444444444444444444444444444444444444444";
        store
            .record_pending(done, PaymentDirection::Outbound, 4, 1_000)
            .unwrap();
        store
            .settle(done, PaymentStatus::Succeeded, Some(1), None)
            .unwrap();

        let live: HashSet<String> = [HASH_B.to_string()].into_iter().collect();
        let interrupted = store.reconcile_pending(&live, now_ms).unwrap();
        assert_eq!(interrupted, 1);

        let orphan = store.get(HASH_A).unwrap();
        assert_eq!(orphan.status, PaymentStatus::Failed);
        assert_eq!(orphan.failure_reason.as_deref(), Some(INTERRUPTED_REASON));
        assert_eq!(store.get(HASH_B).unwrap().status, PaymentStatus::Pending);
        assert_eq!(store.get(young).unwrap().status, PaymentStatus::Pending);
        assert_eq!(store.get(done).unwrap().status, PaymentStatus::Succeeded);

        // Durable: the interruption survives a rebuild.
        drop(store);
        assert_eq!(
            store_in(dir.path()).get(HASH_A).unwrap().status,
            PaymentStatus::Failed
        );
    }

    // ---------- keying edge cases ----------

    /// Amountless-outbound (amount 0 at dispatch) and BOLT12-style random
    /// payment ids (not a hash of anything) key and settle correctly.
    #[test]
    fn amountless_and_random_id_rows_key_and_settle_correctly() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());

        // BOLT12: the PWA keys by 32 random bytes hex (`context.tsx:1047`).
        let random_id = "9f8e7d6c5b4a39281706f5e4d3c2b1a09f8e7d6c5b4a39281706f5e4d3c2b1a0";
        store
            .record_pending(random_id, PaymentDirection::Outbound, 0, 42)
            .unwrap();
        let row = store.get(random_id).unwrap();
        assert_eq!(
            row.amount_msat, 0,
            "amountless dispatch records 0 (PWA parity)"
        );

        // Settle by the SAME id (PaymentSent keys by payment_id, not hash).
        store
            .settle(random_id, PaymentStatus::Succeeded, Some(5), None)
            .unwrap();
        assert_eq!(
            store.get(random_id).unwrap().status,
            PaymentStatus::Succeeded
        );
        assert!(store.get(HASH_A).is_none(), "no stray hash-keyed row");
    }

    // ---------- KTD-7 merge rules ----------

    fn ln(
        id: &str,
        status: PaymentStatus,
        direction: PaymentDirection,
        at: u64,
    ) -> PersistedPayment {
        PersistedPayment {
            payment_hash: id.to_string(),
            direction,
            amount_msat: 150_500,
            status,
            fee_paid_msat: None,
            created_at_ms: at,
            failure_reason: if status == PaymentStatus::Failed {
                Some("no route".to_string())
            } else {
                None
            },
        }
    }

    fn onchain(txid: &str, sent: u64, received: u64, conf: Option<u64>) -> OnchainTxSummary {
        OnchainTxSummary {
            txid: txid.to_string(),
            sent_sats: sent,
            received_sats: received,
            confirmed: conf.is_some(),
            confirmation_time_secs: conf,
            first_seen_secs: None,
        }
    }

    fn close(
        channel_id: &str,
        at: u64,
        status: CloseStatusLabel,
        absorbed: &[&str],
    ) -> CloseRecordSummary {
        CloseRecordSummary {
            channel_id: channel_id.to_string(),
            created_at_ms: at,
            expected_amount_sats: None,
            status,
            absorbed_txids: absorbed.iter().map(|s| s.to_string()).collect(),
        }
    }

    /// KTD-7: failed Lightning rows are hidden from the feed
    /// (`use-transaction-history.ts:77`); pending/succeeded map to
    /// pending/confirmed and carry RAW msat.
    #[test]
    fn merge_hides_failed_lightning_rows() {
        let rows = merge_activity(
            &[
                ln(
                    "aa",
                    PaymentStatus::Succeeded,
                    PaymentDirection::Outbound,
                    3,
                ),
                ln("bb", PaymentStatus::Failed, PaymentDirection::Outbound, 2),
                ln("cc", PaymentStatus::Pending, PaymentDirection::Inbound, 1),
            ],
            &[],
            &[],
        );
        assert_eq!(rows.len(), 2, "the failed row must be hidden");
        assert_eq!(rows[0].id, "aa");
        assert_eq!(rows[0].kind, ActivityKind::Lightning);
        assert_eq!(rows[0].direction, Some(ActivityDirection::Sent));
        assert_eq!(rows[0].status, ActivityStatus::Confirmed);
        assert_eq!(rows[0].amount_msat, Some(150_500), "raw msat, no flooring");
        assert_eq!(rows[0].amount_sats, None);
        assert_eq!(rows[0].payment_hash.as_deref(), Some("aa"));
        assert_eq!(rows[1].id, "cc");
        assert_eq!(rows[1].direction, Some(ActivityDirection::Received));
        assert_eq!(rows[1].status, ActivityStatus::Pending);
    }

    /// KTD-7: on-chain txs absorbed by a close record are skipped
    /// (`use-transaction-history.ts:50,55`); the rest render as net
    /// sent/received with the confirmation-time/first-seen/0 timestamp chain.
    #[test]
    fn merge_skips_close_absorbed_txids_and_nets_onchain_amounts() {
        let mut spend = onchain("feed01", 30_000, 8_000, Some(1_753_000_100));
        spend.first_seen_secs = Some(1_752_999_000);
        let receive_unconf = OnchainTxSummary {
            txid: "feed02".to_string(),
            sent_sats: 0,
            received_sats: 12_345,
            confirmed: false,
            confirmation_time_secs: None,
            first_seen_secs: Some(1_753_000_200),
        };
        let never_seen = OnchainTxSummary {
            txid: "feed03".to_string(),
            sent_sats: 0,
            received_sats: 1,
            confirmed: false,
            confirmation_time_secs: None,
            first_seen_secs: None,
        };
        let absorbed = onchain("c0ffee", 0, 500_000, Some(1_753_999_999));

        let rows = merge_activity(
            &[],
            &[spend, receive_unconf, never_seen, absorbed],
            &[close(
                "chan1",
                1_753_000_000_000,
                CloseStatusLabel::WaitingTimelock,
                &["c0ffee"],
            )],
        );

        let ids: Vec<&str> = rows.iter().map(|r| r.id.as_str()).collect();
        assert!(!ids.contains(&"c0ffee"), "absorbed txid must be skipped");

        let spend_row = rows.iter().find(|r| r.id == "feed01").unwrap();
        assert_eq!(spend_row.kind, ActivityKind::Onchain);
        assert_eq!(spend_row.direction, Some(ActivityDirection::Sent));
        assert_eq!(
            spend_row.amount_sats,
            Some(22_000),
            "net sent = sent - received"
        );
        assert_eq!(spend_row.amount_msat, None);
        assert_eq!(
            spend_row.created_at_ms, 1_753_000_100_000,
            "confirmation time (secs->ms) wins over first-seen"
        );
        assert_eq!(spend_row.status, ActivityStatus::Confirmed);
        assert_eq!(spend_row.txid.as_deref(), Some("feed01"));

        let recv_row = rows.iter().find(|r| r.id == "feed02").unwrap();
        assert_eq!(recv_row.direction, Some(ActivityDirection::Received));
        assert_eq!(recv_row.amount_sats, Some(12_345));
        assert_eq!(
            recv_row.created_at_ms, 1_753_000_200_000,
            "first-seen fallback"
        );
        assert_eq!(recv_row.status, ActivityStatus::Pending);

        let unseen_row = rows.iter().find(|r| r.id == "feed03").unwrap();
        assert_eq!(
            unseen_row.created_at_ms, 0,
            "never-seen txs sort to the bottom"
        );
    }

    /// KTD-7: one row per close record with a stable id/timestamp; only
    /// Complete/ResolvedUnverified read as confirmed
    /// (`use-transaction-history.ts:89-105`).
    #[test]
    fn merge_emits_one_stable_row_per_close_record() {
        let mut resolved = close("chanA", 5_000, CloseStatusLabel::ResolvedUnverified, &[]);
        resolved.expected_amount_sats = Some(72_000);
        let waiting = close("chanB", 6_000, CloseStatusLabel::WaitingTimelock, &[]);

        let rows = merge_activity(&[], &[], &[resolved, waiting]);
        assert_eq!(rows.len(), 2);

        let row_a = rows.iter().find(|r| r.id == "close:chanA").unwrap();
        assert_eq!(row_a.kind, ActivityKind::ChannelClose);
        assert_eq!(row_a.direction, Some(ActivityDirection::Received));
        assert_eq!(row_a.amount_sats, Some(72_000));
        assert_eq!(row_a.status, ActivityStatus::Confirmed);
        assert_eq!(
            row_a.created_at_ms, 5_000,
            "stable sort key — rows must not hop"
        );
        assert_eq!(row_a.channel_id.as_deref(), Some("chanA"));
        assert_eq!(
            row_a.close_status,
            Some(CloseStatusLabel::ResolvedUnverified)
        );

        let row_b = rows.iter().find(|r| r.id == "close:chanB").unwrap();
        assert_eq!(row_b.status, ActivityStatus::Pending);
        assert_eq!(
            row_b.amount_sats, None,
            "unknown close amount must be None, never a lying 0"
        );
    }

    /// KTD-7: the merged feed sorts descending by timestamp across all three
    /// sources (`use-transaction-history.ts:107`).
    #[test]
    fn merge_sorts_descending_across_all_sources() {
        let rows = merge_activity(
            &[ln(
                "aa",
                PaymentStatus::Succeeded,
                PaymentDirection::Inbound,
                2_000,
            )],
            &[onchain("feed01", 0, 1_000, Some(3))], // 3s -> 3_000 ms
            &[close("chanA", 1_000, CloseStatusLabel::Closing, &[])],
        );
        let ids: Vec<&str> = rows.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, vec!["feed01", "aa", "close:chanA"]);
    }
}
