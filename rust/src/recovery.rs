//! Force-close recovery engine (U10; R9 recovery half, R3; KTD-3, F4).
//!
//! Mirrors the PWA's `zinq/src/ldk/recovery/`: a [`RecoveryState`] blob
//! (serde parity with `recovery-state.ts:12-21` — the VSS
//! `force_close_recovery` value must be interchangeable with PWA blobs),
//! entered when a `BumpTransaction` event finds NO confirmed on-chain UTXO
//! for the anchor CPFP — but NEVER before the Initial BDK Scan completes.
//! That gate encodes a real production incident (`onchain/scan-state.ts`):
//! on restore, LDK replays chain-monitor events before the fresh wallet has
//! scanned, `list_unspent()` is empty BY CONSTRUCTION, and the check fired a
//! false "Recover funds" banner on every restore. A genuinely stuck close
//! re-triggers naturally — LDK re-yields bump events each new block.
//!
//! Exit is chain-truth (`recovery-reconcile.ts`): once ANY closing tx
//! CONFIRMED for every recovery channel — ours (fees sufficed) or the
//! counterparty's (ours is superseded and can never confirm) — the CPFP is
//! moot and the deposit ask is wrong; a 10 s tick clears it. The auto-recover
//! sweep attempt (~60 s) transitions to `sweep_confirmed` when U11's sweep
//! engine ([`crate::sweep::SweepEngine`], via the [`RecoverySweeper`] seam)
//! lands funds.

use std::sync::{Arc, Mutex};

use lightning::log_error;
use lightning::log_info;
use lightning::util::logger::Logger as _;
use lightning::util::persist::KVStoreSync;
use lightning_persister::fs_store::FilesystemStore;
use serde::{Deserialize, Serialize};

use crate::close_records::{on_commitment_broadcast, CloseRecord, CloseRecordStore};
use crate::node::{CoreEvent, EventSink};
use crate::types::Logger;
use crate::vss::store::{VssBackedStore, FORCE_CLOSE_RECOVERY_VSS_KEY};

/// Local KVStore location (the PWA's `ldk_force_close_recovery` IDB store).
pub(crate) const RECOVERY_PRIMARY_NAMESPACE: &str = "force_close_recovery";
pub(crate) const RECOVERY_SECONDARY_NAMESPACE: &str = "";
pub(crate) const RECOVERY_LOCAL_KEY: &str = "state";

/// Anchor CPFP typically needs ~140 vbytes (`use-recovery.ts:24`).
pub(crate) const CPFP_VBYTES_ESTIMATE: f64 = 140.0;
/// Deposit rounding step (`recovery-state.ts:103-107`).
pub(crate) const DEPOSIT_STEP_SATS: u64 = 5_000;
/// Safe default when fee estimation fails (`use-recovery.ts:34`).
pub(crate) const DEPOSIT_FALLBACK_SATS: u64 = 25_000;
/// Exit-reconcile tick cadence (`context.tsx`: every peer-timer tick, ~10 s).
pub(crate) const RECOVERY_TICK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(10);
/// Auto-recover sweep attempt every N ticks (~60 s, `context.tsx:1455`).
pub(crate) const AUTO_RECOVER_EVERY_TICKS: u32 = 6;

/// `'needs_recovery' | 'sweep_confirmed'` (`recovery-state.ts:10`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryStatus {
    NeedsRecovery,
    SweepConfirmed,
}

/// The PWA's `RecoveryState` shape (`recovery-state.ts:12-21`): camelCase
/// keys, plain JSON numbers, `stuckBalanceSat` null when unknown.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecoveryState {
    pub status: RecoveryStatus,
    /// Estimated stuck balance; `None` (wire `null`) when unknown — the UI
    /// renders "Unknown", never a false ₿0.
    pub stuck_balance_sat: Option<u64>,
    pub deposit_address: String,
    pub deposit_needed_sat: u64,
    pub channel_ids: Vec<String>,
    /// Unix milliseconds.
    pub created_at: u64,
    pub updated_at: u64,
}

// ---------------------------------------------------------------------------
// Deposit calculation (R9: fee-rate × 140 vB × 1.5, 5,000-sat steps,
// 25,000 fallback)
// ---------------------------------------------------------------------------

/// `roundUpDepositNeeded` (`recovery-state.ts:103-107`): 50% buffer, rounded
/// up to 5,000-sat increments.
pub(crate) fn round_up_deposit_needed(exact_sats: u64) -> u64 {
    let buffered = exact_sats.saturating_mul(3).div_ceil(2); // ceil(x * 1.5)
    buffered.div_ceil(DEPOSIT_STEP_SATS) * DEPOSIT_STEP_SATS
}

/// `estimateDepositNeeded` (`use-recovery.ts:27-36`): the 6-block fee rate ×
/// 140 vB, buffered and stepped; [`DEPOSIT_FALLBACK_SATS`] when estimation
/// fails.
pub(crate) fn estimate_deposit_needed(fee_rate_sat_per_vb: Option<f64>) -> u64 {
    match fee_rate_sat_per_vb {
        Some(rate) if rate.is_finite() && rate > 0.0 => {
            round_up_deposit_needed((rate * CPFP_VBYTES_ESTIMATE).ceil() as u64)
        }
        _ => DEPOSIT_FALLBACK_SATS,
    }
}

// ---------------------------------------------------------------------------
// Exit condition (recovery-reconcile.ts:23-37)
// ---------------------------------------------------------------------------

/// True when EVERY recovery channel has a CONFIRMED closing/commitment tx in
/// its close record (any confirmed close counts — including the
/// counterparty's superseding commitment) or a completed record (completion
/// requires positive evidence, which means the close resolved without our
/// deposit). Missing records or unconfirmed close txs keep recovery active:
/// conservative — never clear a deposit ask we can't disprove. An OWN
/// unconfirmed broadcast is exactly such an unconfirmed tx and does NOT
/// clear.
pub(crate) fn close_confirmed_for_all_channels(
    channel_ids: &[String],
    get_record: impl Fn(&str) -> Option<CloseRecord>,
) -> bool {
    if channel_ids.is_empty() {
        return false;
    }
    channel_ids.iter().all(|channel_id| {
        let Some(record) = get_record(channel_id) else {
            return false;
        };
        if record.completed_at_ms.is_some() {
            return true;
        }
        record.txs.iter().any(|tx| {
            matches!(
                tx.role,
                crate::close_records::CloseTxRole::Closing
                    | crate::close_records::CloseTxRole::Commitment
            ) && tx.confirmed_at_height.is_some()
        })
    })
}

// ---------------------------------------------------------------------------
// Sweep seam (U11)
// ---------------------------------------------------------------------------

/// The auto-recover sweep attempt (U11's `SweepEngine` implements this; the
/// tick calls it every ~60 s while recovery is active). Returns swept
/// outputs — `> 0` transitions the banner to `sweep_confirmed`. Async
/// because a real attempt broadcasts (the engine runs on the node runtime).
pub(crate) trait RecoverySweeper: Send + Sync {
    fn attempt_sweep(&self) -> crate::vss::store::BoxFuture<'_, u64>;
}

/// No-op sweeper (recovery tests exercise the seam without an engine).
#[cfg(test)]
pub(crate) struct NoSweeper;

#[cfg(test)]
impl RecoverySweeper for NoSweeper {
    fn attempt_sweep(&self) -> crate::vss::store::BoxFuture<'_, u64> {
        Box::pin(async { 0 })
    }
}

// ---------------------------------------------------------------------------
// Typed errors
// ---------------------------------------------------------------------------

/// Typed recovery-store failures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveryError {
    /// The state blob failed to serialize or write locally.
    Persist { detail: String },
}

impl std::fmt::Display for RecoveryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RecoveryError::Persist { detail } => {
                write!(f, "failed to persist the recovery state: {detail}")
            }
        }
    }
}

impl std::error::Error for RecoveryError {}

// ---------------------------------------------------------------------------
// Store: local-first + best-effort VSS `force_close_recovery`
// ---------------------------------------------------------------------------

/// The recovery-state store: in-memory copy, local KVStore mirror written
/// FIRST (`recovery-state.ts:65-82` — IDB-first so the banner works during a
/// VSS outage), best-effort VSS blob, `RecoveryStateChanged` on every
/// transition.
pub(crate) struct RecoveryStore {
    state: Mutex<Option<RecoveryState>>,
    kv_store: Arc<FilesystemStore>,
    vss: Mutex<Option<Arc<VssBackedStore>>>,
    event_sink: Arc<dyn EventSink>,
    logger: Arc<Logger>,
}

impl RecoveryStore {
    /// Loads the persisted state (absent/corrupt degrades to `None`).
    pub(crate) fn new(
        kv_store: Arc<FilesystemStore>,
        event_sink: Arc<dyn EventSink>,
        logger: Arc<Logger>,
    ) -> Self {
        let state = kv_store
            .read(
                RECOVERY_PRIMARY_NAMESPACE,
                RECOVERY_SECONDARY_NAMESPACE,
                RECOVERY_LOCAL_KEY,
            )
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok());
        Self {
            state: Mutex::new(state),
            kv_store,
            vss: Mutex::new(None),
            event_sink,
            logger,
        }
    }

    pub(crate) fn attach_vss(&self, vss: Arc<VssBackedStore>) {
        *self.vss.lock().unwrap() = Some(vss);
    }

    pub(crate) fn detach_vss(&self) {
        *self.vss.lock().unwrap() = None;
    }

    /// U4 restore: drop the replaced wallet's state.
    pub(crate) fn reset(&self) {
        *self.state.lock().unwrap() = None;
    }

    pub(crate) fn state(&self) -> Option<RecoveryState> {
        self.state.lock().unwrap().clone()
    }

    /// Init seeding (`recovery-state.ts:43-59`): pull the remote blob into an
    /// EMPTY local store (cross-device restore). An existing local state is
    /// never overwritten.
    pub(crate) fn seed_from_remote(&self, remote_bytes: &[u8]) {
        if self.state.lock().unwrap().is_some() {
            return;
        }
        match serde_json::from_slice::<RecoveryState>(remote_bytes) {
            Ok(state) => {
                if let Err(e) = self.persist_locally(&state) {
                    log_error!(self.logger, "Recovery seed persist failed: {e}");
                }
                *self.state.lock().unwrap() = Some(state);
                self.event_sink.emit(CoreEvent::RecoveryStateChanged);
            }
            Err(e) => log_error!(self.logger, "Corrupt remote recovery state ignored: {e}"),
        }
    }

    fn persist_locally(&self, state: &RecoveryState) -> Result<(), RecoveryError> {
        let bytes = serde_json::to_vec(state).map_err(|e| RecoveryError::Persist {
            detail: format!("serialize: {e}"),
        })?;
        self.kv_store
            .write(
                RECOVERY_PRIMARY_NAMESPACE,
                RECOVERY_SECONDARY_NAMESPACE,
                RECOVERY_LOCAL_KEY,
                bytes,
            )
            .map_err(|e| RecoveryError::Persist {
                detail: e.to_string(),
            })
    }

    /// Local-first write + best-effort VSS (LWW like the PWA's
    /// `vssWriteWithConflictRetry`) + change event.
    fn write_state(&self, state: RecoveryState) {
        if let Err(e) = self.persist_locally(&state) {
            log_error!(self.logger, "Recovery local persist failed: {e}");
        }
        if let Some(vss) = self.vss.lock().unwrap().clone() {
            vss.put_lww(
                FORCE_CLOSE_RECOVERY_VSS_KEY,
                serde_json::to_vec(&state).expect("state serializes"),
            );
        }
        *self.state.lock().unwrap() = Some(state);
        self.event_sink.emit(CoreEvent::RecoveryStateChanged);
    }

    /// Entry (`enterRecovery`, use-recovery.ts:42-82): create the state, or
    /// aggregate a NEW channel into an existing one. An unknown balance on
    /// either side poisons the sum — a partial total displayed as THE stuck
    /// balance would understate what's recoverable, so it stays `None`
    /// rather than pretend precision. Re-entering an already-listed channel
    /// is a no-op (event-replay idempotency).
    pub(crate) fn enter(
        &self,
        channel_id: &str,
        local_balance_sat: Option<u64>,
        deposit_address: impl FnOnce() -> Option<String>,
        fee_rate_sat_per_vb: Option<f64>,
        now_ms: u64,
    ) {
        let existing = self.state();
        match existing {
            Some(existing) => {
                if existing.channel_ids.iter().any(|id| id == channel_id) {
                    return;
                }
                let mut updated = existing.clone();
                updated.channel_ids.push(channel_id.to_string());
                updated.stuck_balance_sat = match (existing.stuck_balance_sat, local_balance_sat) {
                    (Some(total), Some(add)) => Some(total + add),
                    _ => None, // unknown poisons the sum
                };
                updated.deposit_needed_sat = estimate_deposit_needed(fee_rate_sat_per_vb);
                updated.updated_at = now_ms;
                self.write_state(updated);
            }
            None => {
                let address = deposit_address().unwrap_or_else(|| {
                    log_error!(
                        self.logger,
                        "No deposit address available for recovery entry"
                    );
                    String::new()
                });
                self.write_state(RecoveryState {
                    status: RecoveryStatus::NeedsRecovery,
                    stuck_balance_sat: local_balance_sat,
                    deposit_address: address,
                    deposit_needed_sat: estimate_deposit_needed(fee_rate_sat_per_vb),
                    channel_ids: vec![channel_id.to_string()],
                    created_at: now_ms,
                    updated_at: now_ms,
                });
            }
        }
    }

    /// Clear (`clearRecoveryState`): local delete + best-effort VSS delete +
    /// change event. Called by the exit tick and by a user dismissing the
    /// success banner.
    pub(crate) fn clear(&self) {
        if self.state.lock().unwrap().take().is_none() {
            return;
        }
        if let Err(e) = self.kv_store.remove(
            RECOVERY_PRIMARY_NAMESPACE,
            RECOVERY_SECONDARY_NAMESPACE,
            RECOVERY_LOCAL_KEY,
            false,
        ) {
            log_error!(self.logger, "Recovery local clear failed: {e}");
        }
        if let Some(vss) = self.vss.lock().unwrap().clone() {
            vss.delete_best_effort(FORCE_CLOSE_RECOVERY_VSS_KEY);
        }
        self.event_sink.emit(CoreEvent::RecoveryStateChanged);
    }

    /// Sweep landed (`context.tsx:1476-1481`): banner flips to
    /// `sweep_confirmed`; the user dismisses it via [`RecoveryStore::clear`].
    pub(crate) fn mark_sweep_confirmed(&self, now_ms: u64) {
        let Some(state) = self.state() else {
            return;
        };
        if state.status == RecoveryStatus::SweepConfirmed {
            return;
        }
        let mut updated = state;
        updated.status = RecoveryStatus::SweepConfirmed;
        updated.updated_at = now_ms;
        self.write_state(updated);
    }

    /// Exit reconcile (`context.tsx:1404-1449`, runs EVERY 10 s tick —
    /// cheap): once a closing tx CONFIRMED for every recovery channel, the
    /// deposit is moot — clear the false "deposit needed" state. This is how
    /// a restore-time false positive heals itself. Returns whether it
    /// cleared.
    pub(crate) fn maybe_clear_resolved(&self, close_records: &CloseRecordStore) -> bool {
        let Some(state) = self.state() else {
            return false;
        };
        if state.status == RecoveryStatus::SweepConfirmed {
            return false;
        }
        if close_confirmed_for_all_channels(&state.channel_ids, |id| close_records.get(id)) {
            log_info!(
                self.logger,
                "Closing tx confirmed for all recovery channels — CPFP no longer needed, \
                 clearing recovery state"
            );
            self.clear();
            return true;
        }
        false
    }

    /// Auto-recovery (`context.tsx:1452-1495`, every ~60 s): attempt the
    /// sweep; swept > 0 transitions to `sweep_confirmed`.
    pub(crate) async fn maybe_auto_recover(&self, sweeper: &dyn RecoverySweeper, now_ms: u64) {
        let Some(state) = self.state() else {
            return;
        };
        if state.status == RecoveryStatus::SweepConfirmed {
            return;
        }
        let swept = sweeper.attempt_sweep().await;
        if swept > 0 {
            log_info!(self.logger, "Auto-sweep recovered {swept} output(s)");
            self.mark_sweep_confirmed(now_ms);
        }
    }
}

// ---------------------------------------------------------------------------
// BumpTransaction observation (event-handler.ts:659-766)
// ---------------------------------------------------------------------------

/// Observes a `BumpTransaction` event for close records + recovery entry.
/// This is deliberately only the OBSERVATION half — the CPFP handling itself
/// is U11's (`BumpTransactionEventHandler`); U10 must not consume the event
/// differently, so the node arm calls this and then acks (LDK re-yields bump
/// events each new block until the claim confirms, so U11's handler still
/// sees the need).
///
/// - `commitment`: the anchor path's (commitment txid, pre-committed fee) —
///   attached to the close record (signals.ts `commitment_broadcast`).
/// - Recovery entry fires when there is NO confirmed UTXO for the CPFP, but
///   NEVER before the Initial Scan completes: on a restore the wallet is
///   empty by construction until the scan lands, so "no UTXOs" is
///   meaningless (the PWA's false-positive incident; plan U10 test
///   scenario). Replays are absorbed by [`RecoveryStore::enter`]'s
///   idempotency.
#[allow(clippy::too_many_arguments)]
pub(crate) fn observe_bump_transaction(
    close_records: &Arc<CloseRecordStore>,
    recovery: &RecoveryStore,
    channel_id_hex: &str,
    commitment: Option<(String, u64)>,
    has_confirmed_utxo: bool,
    initial_scan_complete: bool,
    deposit_address: impl FnOnce() -> Option<String>,
    fee_rate_sat_per_vb: Option<f64>,
    now_ms: u64,
    logger: &Arc<Logger>,
) {
    // Close-record sync read model: replays after a restart still find their
    // context here (records load from local storage before events replay).
    // Null (not 0) when the record is missing or predates the balance fact.
    let local_balance_sat = close_records
        .get(channel_id_hex)
        .and_then(|record| record.expected_amount_sats);

    // Attach the commitment txid + pre-committed fee (anchor path only).
    if let Some((txid, fee_sats)) = &commitment {
        on_commitment_broadcast(close_records, channel_id_hex, txid, *fee_sats, now_ms);
    }

    if !has_confirmed_utxo {
        if initial_scan_complete {
            recovery.enter(
                channel_id_hex,
                local_balance_sat,
                deposit_address,
                fee_rate_sat_per_vb,
                now_ms,
            );
        } else {
            log_info!(
                logger,
                "BumpTransaction: no UTXOs but initial scan pending — deferring recovery signal"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::close_records::{CloseOutpoint, CloseRecordTx, CloseTxRole, CloseType};
    use std::path::Path;

    #[derive(Default)]
    struct CapturingSink(Mutex<Vec<CoreEvent>>);

    impl EventSink for CapturingSink {
        fn emit(&self, event: CoreEvent) {
            self.0.lock().unwrap().push(event);
        }
    }

    fn recovery_in(dir: &Path, sink: Arc<CapturingSink>) -> RecoveryStore {
        RecoveryStore::new(
            Arc::new(FilesystemStore::new(dir.join("store"))),
            sink,
            Arc::new(Logger),
        )
    }

    fn close_store_in(dir: &Path) -> Arc<crate::close_records::CloseRecordStore> {
        Arc::new(crate::close_records::CloseRecordStore::new(
            Arc::new(FilesystemStore::new(dir.join("store"))),
            Arc::new(Logger),
        ))
    }

    fn record_with_tx(
        channel_id: &str,
        role: CloseTxRole,
        confirmed_at_height: Option<u32>,
    ) -> CloseRecord {
        let mut record = CloseRecord::skeleton(channel_id, 1_000);
        record.txs.push(CloseRecordTx {
            txid: format!("{channel_id}-tx"),
            role,
            fee_sats: None,
            confirmed_at_height,
        });
        record
    }

    fn events_of(sink: &CapturingSink) -> Vec<CoreEvent> {
        sink.0.lock().unwrap().clone()
    }

    // ---------- deposit calc (R9 table) ----------

    /// R9: fee-rate × 140 vB × 1.5, ceil'd to 5,000-sat steps; 25,000 when
    /// estimation fails. Cases mirror `recovery-state.ts:103-107` +
    /// `use-recovery.ts:27-36`.
    #[test]
    fn deposit_calc_matches_the_pwa_table() {
        // round_up: ceil(x*1.5) then ceil to 5000.
        assert_eq!(round_up_deposit_needed(700), 5_000); // 1050 → 5000
        assert_eq!(round_up_deposit_needed(3_333), 5_000); // 5000 exactly
        assert_eq!(round_up_deposit_needed(3_334), 10_000); // 5001 → 10000
        assert_eq!(round_up_deposit_needed(10_000), 15_000);
        assert_eq!(round_up_deposit_needed(20_020), 35_000); // 30030 → 35000

        // estimate: rate × 140 vB through the same rounding.
        assert_eq!(estimate_deposit_needed(Some(5.0)), 5_000); // 700 → 1050
        assert_eq!(estimate_deposit_needed(Some(20.0)), 5_000); // 2800 → 4200
        assert_eq!(estimate_deposit_needed(Some(100.0)), 25_000); // 14000 → 21000
        assert_eq!(estimate_deposit_needed(Some(143.0)), 35_000); // 20020 → 30030
        assert_eq!(estimate_deposit_needed(Some(0.5)), 5_000); // 70 → 105
                                                               // Fee estimation failed → the safe default.
        assert_eq!(estimate_deposit_needed(None), 25_000);
        assert_eq!(estimate_deposit_needed(Some(f64::NAN)), 25_000);
        assert_eq!(estimate_deposit_needed(Some(0.0)), 25_000);
    }

    // ---------- entry gating (the plan's named invariants) ----------

    /// THE FALSE-POSITIVE INCIDENT (plan U10, mandatory): a replayed
    /// BumpTransaction after restore must NOT enter recovery when a
    /// confirmed UTXO exists — the wallet can pay for the CPFP.
    #[test]
    fn replayed_bump_with_a_confirmed_utxo_does_not_enter_recovery() {
        let dir = tempfile::tempdir().unwrap();
        let sink = Arc::new(CapturingSink::default());
        let recovery = recovery_in(dir.path(), Arc::clone(&sink));
        let close_records = close_store_in(dir.path());

        observe_bump_transaction(
            &close_records,
            &recovery,
            "chan1",
            Some(("commit1".into(), 2_000)),
            true, // confirmed UTXO exists
            true, // scan complete
            || Some("bc1qaddr".into()),
            Some(5.0),
            1_000,
            &Arc::new(Logger),
        );

        assert!(
            recovery.state().is_none(),
            "a funded wallet must never see the deposit banner"
        );
        assert!(events_of(&sink).is_empty());
        // The commitment fact was still recorded (the observation half).
        assert!(close_records.get("chan1").is_some());
    }

    /// NO recovery state before the Initial Scan completes, EVER (plan U10,
    /// mandatory): "no UTXOs" is meaningless while the wallet is empty by
    /// construction.
    #[test]
    fn no_recovery_state_before_initial_scan_completes_ever() {
        let dir = tempfile::tempdir().unwrap();
        let sink = Arc::new(CapturingSink::default());
        let recovery = recovery_in(dir.path(), Arc::clone(&sink));
        let close_records = close_store_in(dir.path());

        for _ in 0..3 {
            observe_bump_transaction(
                &close_records,
                &recovery,
                "chan1",
                None,
                false, // no confirmed UTXO...
                false, // ...but the scan hasn't completed
                || Some("bc1qaddr".into()),
                Some(5.0),
                1_000,
                &Arc::new(Logger),
            );
        }

        assert!(
            recovery.state().is_none(),
            "recovery must never be entered before the initial scan"
        );
        assert!(events_of(&sink).is_empty());
    }

    /// The genuine case: scan complete, no confirmed UTXO → recovery with
    /// the close record's balance, the deposit address, and the calculated
    /// deposit. Replays are idempotent.
    #[test]
    fn genuine_stuck_close_enters_recovery_once() {
        let dir = tempfile::tempdir().unwrap();
        let sink = Arc::new(CapturingSink::default());
        let recovery = recovery_in(dir.path(), Arc::clone(&sink));
        let close_records = close_store_in(dir.path());
        let mut record = CloseRecord::skeleton("chan1", 500);
        record.expected_amount_sats = Some(80_000);
        close_records.upsert(record);

        for _ in 0..2 {
            observe_bump_transaction(
                &close_records,
                &recovery,
                "chan1",
                Some(("commit1".into(), 2_000)),
                false,
                true,
                || Some("bc1qaddr".into()),
                Some(20.0),
                1_000,
                &Arc::new(Logger),
            );
        }

        let state = recovery.state().expect("recovery entered");
        assert_eq!(state.status, RecoveryStatus::NeedsRecovery);
        assert_eq!(state.stuck_balance_sat, Some(80_000));
        assert_eq!(state.deposit_address, "bc1qaddr");
        assert_eq!(state.deposit_needed_sat, 5_000); // 20 sat/vB case
        assert_eq!(state.channel_ids, vec!["chan1"]);
        assert_eq!(state.created_at, 1_000);
        assert_eq!(
            events_of(&sink)
                .iter()
                .filter(|e| matches!(e, CoreEvent::RecoveryStateChanged))
                .count(),
            1,
            "the replay must not re-write or re-notify"
        );
    }

    /// Multi-channel accumulation: a second channel joins the existing
    /// state; an unknown balance on either side poisons the sum to None.
    #[test]
    fn multi_channel_accumulation_and_unknown_poisons_the_sum() {
        let dir = tempfile::tempdir().unwrap();
        let sink = Arc::new(CapturingSink::default());
        let recovery = recovery_in(dir.path(), Arc::clone(&sink));

        recovery.enter(
            "chan1",
            Some(30_000),
            || Some("bc1qaddr".into()),
            Some(5.0),
            1_000,
        );
        recovery.enter(
            "chan2",
            Some(20_000),
            || Some("unused".into()),
            Some(5.0),
            2_000,
        );
        let state = recovery.state().unwrap();
        assert_eq!(state.channel_ids, vec!["chan1", "chan2"]);
        assert_eq!(state.stuck_balance_sat, Some(50_000), "known balances sum");
        assert_eq!(
            state.deposit_address, "bc1qaddr",
            "the first address sticks"
        );
        assert_eq!(state.created_at, 1_000);
        assert_eq!(state.updated_at, 2_000);

        recovery.enter("chan3", None, || Some("unused".into()), Some(5.0), 3_000);
        assert_eq!(
            recovery.state().unwrap().stuck_balance_sat,
            None,
            "unknown poisons the sum — never pretend precision"
        );
        // And once poisoned it stays poisoned.
        recovery.enter(
            "chan4",
            Some(1_000),
            || Some("unused".into()),
            Some(5.0),
            4_000,
        );
        assert_eq!(recovery.state().unwrap().stuck_balance_sat, None);
    }

    // ---------- exit reconciliation (chain truth) ----------

    /// THE SUPERSEDED-COMMITMENT EXIT (plan U10, mandatory): the
    /// counterparty's CONFIRMED commitment clears recovery — our commitment
    /// can never confirm, the CPFP is moot. An OWN UNCONFIRMED broadcast
    /// does NOT clear (never clear a deposit ask we can't disprove).
    #[test]
    fn counterparty_confirmed_commitment_clears_recovery_own_unconfirmed_does_not() {
        let dir = tempfile::tempdir().unwrap();
        let sink = Arc::new(CapturingSink::default());
        let recovery = recovery_in(dir.path(), Arc::clone(&sink));
        let close_records = close_store_in(dir.path());
        recovery.enter(
            "chan1",
            Some(10_000),
            || Some("addr".into()),
            Some(5.0),
            1_000,
        );

        // Own broadcast-time commitment, unconfirmed: recovery persists.
        close_records.upsert(record_with_tx("chan1", CloseTxRole::Commitment, None));
        assert!(!recovery.maybe_clear_resolved(&close_records));
        assert!(
            recovery.state().is_some(),
            "own unconfirmed broadcast must not clear"
        );

        // The counterparty's commitment confirms (recorded by reconcile's
        // funding-outspend discovery): recovery clears.
        let mut theirs = CloseRecord::skeleton("chan1", 1_000);
        theirs.txs.push(CloseRecordTx {
            txid: "theirs1".into(),
            role: CloseTxRole::Commitment,
            fee_sats: None,
            confirmed_at_height: Some(90),
        });
        close_records.upsert(theirs);
        assert!(recovery.maybe_clear_resolved(&close_records));
        assert!(
            recovery.state().is_none(),
            "confirmed close → deposit ask cleared"
        );
        // Cleared durably.
        assert!(recovery_in(dir.path(), Arc::new(CapturingSink::default()))
            .state()
            .is_none());
    }

    /// Missing records and multi-channel partial confirmation keep recovery
    /// active; a COMPLETED record counts as confirmed.
    #[test]
    fn exit_requires_every_channel_confirmed_missing_records_block() {
        let dir = tempfile::tempdir().unwrap();
        let sink = Arc::new(CapturingSink::default());
        let recovery = recovery_in(dir.path(), Arc::clone(&sink));
        let close_records = close_store_in(dir.path());
        recovery.enter("chan1", None, || Some("addr".into()), Some(5.0), 1_000);
        recovery.enter("chan2", None, || Some("addr".into()), Some(5.0), 1_000);

        // chan1 confirmed, chan2 has NO record at all → stays active.
        close_records.upsert(record_with_tx("chan1", CloseTxRole::Commitment, Some(90)));
        assert!(!recovery.maybe_clear_resolved(&close_records));
        assert!(recovery.state().is_some());

        // chan2 completes (positive evidence) → clears.
        let mut done = CloseRecord::skeleton("chan2", 1_000);
        done.completed_at_ms = Some(5_000);
        close_records.upsert(done);
        assert!(recovery.maybe_clear_resolved(&close_records));
        assert!(recovery.state().is_none());
    }

    /// `closeConfirmedForAllChannels` edge: an empty channel list never
    /// clears (recovery-reconcile.ts:27).
    #[test]
    fn empty_channel_list_never_reads_as_confirmed() {
        assert!(!close_confirmed_for_all_channels(&[], |_| None));
    }

    /// A coop `closing` tx counts exactly like a `commitment`.
    #[test]
    fn confirmed_coop_closing_tx_also_clears() {
        let dir = tempfile::tempdir().unwrap();
        let close_records = close_store_in(dir.path());
        close_records.upsert(record_with_tx("chan1", CloseTxRole::Closing, Some(90)));
        assert!(close_confirmed_for_all_channels(
            &["chan1".to_string()],
            |id| close_records.get(id)
        ));
    }

    // ---------- sweep_confirmed + auto-recover seam ----------

    struct FixedSweeper(u64);
    impl RecoverySweeper for FixedSweeper {
        fn attempt_sweep(&self) -> crate::vss::store::BoxFuture<'_, u64> {
            let swept = self.0;
            Box::pin(async move { swept })
        }
    }

    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(future)
    }

    #[test]
    fn auto_recover_transitions_to_sweep_confirmed_on_swept_funds() {
        let dir = tempfile::tempdir().unwrap();
        let sink = Arc::new(CapturingSink::default());
        let recovery = recovery_in(dir.path(), Arc::clone(&sink));
        recovery.enter(
            "chan1",
            Some(10_000),
            || Some("addr".into()),
            Some(5.0),
            1_000,
        );

        // The no-op sweeper never transitions.
        block_on(recovery.maybe_auto_recover(&NoSweeper, 2_000));
        assert_eq!(
            recovery.state().unwrap().status,
            RecoveryStatus::NeedsRecovery
        );

        block_on(recovery.maybe_auto_recover(&FixedSweeper(3), 3_000));
        let state = recovery.state().unwrap();
        assert_eq!(state.status, RecoveryStatus::SweepConfirmed);
        assert_eq!(state.updated_at, 3_000);

        // sweep_confirmed is terminal for the tick: no more attempts, and the
        // exit reconcile leaves the success banner alone.
        block_on(recovery.maybe_auto_recover(&FixedSweeper(1), 4_000));
        assert_eq!(recovery.state().unwrap().updated_at, 3_000);
        let close_records = close_store_in(dir.path());
        assert!(!recovery.maybe_clear_resolved(&close_records));
        assert!(
            recovery.state().is_some(),
            "the success banner persists until dismissed"
        );
    }

    // ---------- persistence + VSS blob parity ----------

    /// The persisted blob has the PWA's exact wire shape (`recovery-state.ts`
    /// JSON.stringify): camelCase keys, plain numbers, null stuck balance.
    #[test]
    fn persisted_blob_matches_the_pwa_wire_shape() {
        let dir = tempfile::tempdir().unwrap();
        let sink = Arc::new(CapturingSink::default());
        let recovery = recovery_in(dir.path(), Arc::clone(&sink));
        recovery.enter(
            "chan1",
            None,
            || Some("bc1qaddr".into()),
            None,
            1_753_000_000_000,
        );

        let bytes = FilesystemStore::new(dir.path().join("store"))
            .read(
                RECOVERY_PRIMARY_NAMESPACE,
                RECOVERY_SECONDARY_NAMESPACE,
                RECOVERY_LOCAL_KEY,
            )
            .unwrap();
        let blob: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(blob["status"], "needs_recovery");
        assert_eq!(blob["stuckBalanceSat"], serde_json::Value::Null);
        assert_eq!(blob["depositAddress"], "bc1qaddr");
        assert_eq!(blob["depositNeededSat"], 25_000);
        assert_eq!(blob["channelIds"], serde_json::json!(["chan1"]));
        assert_eq!(blob["createdAt"], 1_753_000_000_000u64);
        assert_eq!(blob["updatedAt"], 1_753_000_000_000u64);

        // And a PWA-written blob loads unchanged.
        let reloaded = recovery_in(dir.path(), Arc::new(CapturingSink::default()));
        assert_eq!(reloaded.state(), recovery.state());
    }

    /// Cross-device seeding: a remote blob fills an EMPTY local store; an
    /// existing local state is never overwritten.
    #[test]
    fn seed_from_remote_fills_only_an_empty_store() {
        let dir = tempfile::tempdir().unwrap();
        let sink = Arc::new(CapturingSink::default());
        let recovery = recovery_in(dir.path(), Arc::clone(&sink));

        let remote = serde_json::json!({
            "status": "needs_recovery",
            "stuckBalanceSat": 12_345,
            "depositAddress": "bc1qremote",
            "depositNeededSat": 10_000,
            "channelIds": ["chanR"],
            "createdAt": 5_000,
            "updatedAt": 6_000,
        });
        recovery.seed_from_remote(&serde_json::to_vec(&remote).unwrap());
        let state = recovery.state().expect("seeded");
        assert_eq!(state.stuck_balance_sat, Some(12_345));
        assert_eq!(state.deposit_address, "bc1qremote");

        // A second (different) remote must not overwrite.
        let other = serde_json::json!({
            "status": "sweep_confirmed",
            "stuckBalanceSat": null,
            "depositAddress": "bc1qother",
            "depositNeededSat": 5_000,
            "channelIds": ["chanX"],
            "createdAt": 1, "updatedAt": 2,
        });
        recovery.seed_from_remote(&serde_json::to_vec(&other).unwrap());
        assert_eq!(recovery.state().unwrap().deposit_address, "bc1qremote");
    }

    /// U4 restore: the replaced wallet's recovery banner must not survive.
    #[test]
    fn reset_drops_the_cached_state() {
        let dir = tempfile::tempdir().unwrap();
        let sink = Arc::new(CapturingSink::default());
        let recovery = recovery_in(dir.path(), Arc::clone(&sink));
        recovery.enter("chan1", None, || Some("addr".into()), Some(5.0), 1_000);
        recovery.reset();
        assert!(recovery.state().is_none());
    }

    /// The commitment fact recorded during observation carries the funding
    /// outpoint context needed later (regression guard for the observation
    /// half staying independent of recovery entry).
    #[test]
    fn observation_records_commitment_even_when_recovery_is_not_entered() {
        let dir = tempfile::tempdir().unwrap();
        let sink = Arc::new(CapturingSink::default());
        let recovery = recovery_in(dir.path(), Arc::clone(&sink));
        let close_records = close_store_in(dir.path());
        let mut record = CloseRecord::skeleton("chan1", 500);
        record.funding_txo = Some(CloseOutpoint {
            txid: "fund1".into(),
            vout: 0,
        });
        close_records.upsert(record);

        observe_bump_transaction(
            &close_records,
            &recovery,
            "chan1",
            Some(("commit1".into(), 3_000)),
            false,
            false, // pre-scan: no recovery...
            || Some("addr".into()),
            Some(5.0),
            1_000,
            &Arc::new(Logger),
        );

        let record = close_records.get("chan1").unwrap();
        assert!(
            record.txs.iter().any(|tx| tx.txid == "commit1"
                && tx.fee_sats == Some(3_000)
                && tx.role == CloseTxRole::Commitment),
            "...but the commitment fact is still recorded"
        );
        assert_eq!(record.close_type, CloseType::Force);
        assert!(recovery.state().is_none());
    }
}
