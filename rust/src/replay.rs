//! Close-record-driven replay of missed spendable outputs (R8/R9, F4; the
//! U10 ↔ U11 seam), run once per boot after the initial chain sync.
//!
//! ## The gap this closes
//!
//! LDK hands us force-close proceeds exactly once, as an `Event::SpendableOutputs`
//! that U11's [`crate::sweep::SweepEngine::track_spendable_outputs`] persists
//! into the local `spendable_outputs` KVStore namespace. That namespace is
//! **local-only** — it is not in the VSS key set (`vss/store.rs`), and the PWA
//! never backed it up either. So a wallet restored from a seed another client
//! created (F3/R4) inherits `monitors`, `manager`, `close_records`,
//! `known_peers` and `payment_history` — and an EMPTY sweep queue. LDK will not
//! re-emit the event, because the client that created the wallet already
//! consumed it: `SpendableOutputs` is replayed only while UNRESOLVED, and it
//! was resolved (acknowledged) on the other device.
//!
//! The result observed on a real mainnet wallet: a remote-initiated force close
//! whose restored close record says `closeType: force`, `initiator: remote`,
//! `expectedAmountSats: 11500`, with only the commitment tx attached — no
//! `sweep` tx, no `resolution`, no `completedAt` — while output 2 of that
//! commitment sat unspent on chain. The money was claimable, the record proved
//! it was owed, and nothing in the app noticed. The close record is the only
//! piece of durable, cross-client evidence that survives the restore, so it is
//! what drives the recovery.
//!
//! ## The mechanism
//!
//! [`ChannelMonitor::get_spendable_outputs`] exists for precisely this case —
//! its own docs say it "serves as a way to retrieve these descriptors at a
//! later time, either for historical purposes, or to replay any
//! missed/unhandled descriptors". It re-derives the descriptors from a
//! transaction's outputs and keeps only those with at least
//! `max(ANTI_REORG_DELAY, to_self_delay)` confirmations relative to the
//! monitor's `best_block`, which is why this pass runs AFTER the builder's
//! initial `sync_confirmables` (`builder.rs`): an unsynced monitor would
//! measure confirmations against a stale tip.
//!
//! ## Why the already-spent guard is load-bearing
//!
//! `get_spendable_outputs` is a pure re-derivation: it answers "could we spend
//! this output", NOT "is this output still there". On the same wallet, a second
//! channel's 2,500-sat output had already been swept — re-tracking it would
//! have left a permanently unspendable member in U11's all-or-nothing batch,
//! and since one bad member fails EVERY sweep, that is the exact fund-freeze
//! shape of PR #177's wallet-owned-`StaticOutput` incident
//! (`zinq/docs/solutions/integration-issues/ldk-spendable-output-sweep-stuck-retry-and-fee-semantics.md`).
//! So every candidate outpoint is checked against Esplora's outspend endpoint
//! first, and an ERROR is treated as "do not track" — never as "unspent".
//! Esplora reports mempool spends too, so an in-flight sweep from another
//! device is caught by the same guard.
//!
//! ## What this deliberately does NOT do
//!
//! It does not create a second sweep queue, and it does not enter U10's
//! [`crate::recovery::RecoveryState`]. Everything downstream is the existing
//! engine: dedup by descriptor hex + outpoint, the wallet-owned `StaticOutput`
//! exclusion, the retry cadence, the subsidized fallback, and the
//! `pending_sweep()` / `PendingSweepInfo` surface. Recovery state is the
//! anchor-CPFP DEPOSIT ask ("A small deposit is needed to unlock them",
//! `zinq/src/components/RecoveryBanner.tsx`), entered only from a
//! `BumpTransaction` with no confirmed UTXO; asking for a deposit to sweep a
//! plain unspent commitment output would recreate the false-positive incident
//! in
//! `zinq/docs/solutions/logic-errors/force-close-recovery-false-positive-on-vss-restore.md`.
//!
//! [`ChannelMonitor::get_spendable_outputs`]: lightning::chain::channelmonitor::ChannelMonitor::get_spendable_outputs

use std::sync::Arc;

use bitcoin::Transaction;
use lightning::log_error;
use lightning::log_info;
use lightning::sign::SpendableOutputDescriptor;
use lightning::util::logger::Logger as _;

use crate::close_records::{
    ChainTruth, CloseOutpoint, CloseRecord, CloseRecordStore, CloseRecordTx, CloseTxRole,
    CloseType, Resolution,
};
use crate::recovery::close_confirmed_for_all_channels;
use crate::sweep::SweepEngine;
use crate::types::Logger;

// ---------------------------------------------------------------------------
// Monitor seam
// ---------------------------------------------------------------------------

/// The monitor-side half of the replay: [`crate::types::ChainMonitor`] in
/// production, a fake in tests.
///
/// A `ChannelMonitor` cannot be minted offline (it needs a real funded
/// channel — see `fixtures/channel_monitor_vectors.json`'s `_why`), and the
/// fixture monitors are for OPEN channels, so they can never yield a
/// force-close descriptor. This seam is what makes the pass's own logic —
/// the trigger predicate, the tx walk, the already-spent guard, idempotency —
/// testable offline without weakening the production path, which is a thin
/// pass-through to LDK.
pub(crate) trait MonitorDescriptors: Send + Sync {
    /// Whether a monitor for `funding_txo` is loaded. Cheap and in-memory:
    /// checked BEFORE any Esplora work so a record whose monitor was archived
    /// costs nothing.
    fn has_monitor(&self, funding_txo: &CloseOutpoint) -> bool;

    /// [`lightning::chain::channelmonitor::ChannelMonitor::get_spendable_outputs`]
    /// for the monitor whose funding outpoint is `funding_txo`; `None` when no
    /// such monitor is loaded.
    fn replay_spendable_outputs(
        &self,
        funding_txo: &CloseOutpoint,
        tx: &Transaction,
        confirmation_height: u32,
    ) -> Option<Vec<SpendableOutputDescriptor>>;
}

impl MonitorDescriptors for crate::types::ChainMonitor {
    fn has_monitor(&self, funding_txo: &CloseOutpoint) -> bool {
        self.list_monitors().into_iter().any(|channel_id| {
            self.get_monitor(channel_id)
                .is_ok_and(|monitor| funding_txo_matches(&monitor.get_funding_txo(), funding_txo))
        })
    }

    fn replay_spendable_outputs(
        &self,
        funding_txo: &CloseOutpoint,
        tx: &Transaction,
        confirmation_height: u32,
    ) -> Option<Vec<SpendableOutputDescriptor>> {
        for channel_id in self.list_monitors() {
            let Ok(monitor) = self.get_monitor(channel_id) else {
                continue;
            };
            if funding_txo_matches(&monitor.get_funding_txo(), funding_txo) {
                return Some(monitor.get_spendable_outputs(tx, confirmation_height));
            }
        }
        None
    }
}

/// Close records store funding outpoints in DISPLAY txid hex (Esplora's byte
/// order), which is exactly what LDK's `Txid: Display` produces — the same
/// convention `close_records`' reconcile already relies on.
fn funding_txo_matches(
    monitor_txo: &lightning::chain::transaction::OutPoint,
    record_txo: &CloseOutpoint,
) -> bool {
    monitor_txo.index as u32 == record_txo.vout && monitor_txo.txid.to_string() == record_txo.txid
}

// ---------------------------------------------------------------------------
// Trigger predicate
// ---------------------------------------------------------------------------

/// Whether a close record still shows force-close funds that were never
/// swept, and is therefore worth spending Esplora queries on.
///
/// Every clause is a fact already carried by the PWA-normative record shape;
/// nothing here re-derives status (that is [`crate::close_records::derive_close_status`]'s
/// job). A record failing this test costs ZERO network calls, so the
/// steady-state wallet with nothing outstanding pays nothing to run this pass
/// on every boot.
///
/// - `force`: a coop close pays the bdk wallet directly (U1's signer hands LDK
///   wallet-derived scripts), so it produces no descriptor to sweep. `unknown`
///   is excluded for the same reason plus honesty: the offline-close safety-net
///   record (`reconcile.ts:143`) carries no balance fact to act on.
/// - `expected_amount_sats > 0`: LDK's last-known local balance at close. Zero
///   means nothing was owed; absent means we never captured it, and a record
///   with no claim on record is not evidence of money owed.
/// - no CONFIRMED `sweep`-role tx: a confirmed sweep is positive proof the
///   funds already came home (KTD-7 attribution puts the txid on every
///   contributing channel's record). An UNCONFIRMED sweep does not disqualify
///   the record — it may never confirm — because the already-spent guard
///   covers that case precisely: an in-flight sweep spends the outpoint, and
///   Esplora reports mempool spends.
/// - not `resolution: verified`: verified completion means the funds were
///   observed arriving in OUR wallet. `resolved_unverified` deliberately does
///   NOT disqualify: that state means "the close resolved on chain but our
///   wallet never saw the funds" (`reconcile.ts` positive-evidence-only
///   completion), which is indistinguishable from this very bug — so it is
///   re-examined, and the guard makes re-examination safe.
/// - a close tx exists: [`close_confirmed_for_all_channels`] — recovery's own
///   exit predicate, reused rather than re-implemented — is the cheap
///   in-memory signal. When the mirror does not yet know a height (a record
///   restored from another client can carry the commitment txid with no
///   `confirmedAtHeight` until the first reconcile pass fills it in), naming a
///   close tx is enough: the pass resolves the height from chain truth itself,
///   so a one-shot-per-boot recovery never needs a second launch.
pub(crate) fn needs_descriptor_replay(record: &CloseRecord) -> bool {
    if record.close_type != CloseType::Force {
        return false;
    }
    if record.funding_txo.is_none() {
        return false;
    }
    if record.expected_amount_sats.is_none_or(|sats| sats == 0) {
        return false;
    }
    if record.resolution == Some(Resolution::Verified) {
        return false;
    }
    let swept = record
        .txs
        .iter()
        .any(|tx| tx.role == CloseTxRole::Sweep && tx.confirmed_at_height.is_some());
    if swept {
        return false;
    }
    let close_confirmed_locally =
        close_confirmed_for_all_channels(std::slice::from_ref(&record.channel_id), |_| {
            Some(record.clone())
        });
    close_confirmed_locally || record.txs.iter().any(|tx| tx.role.is_close())
}

/// The transactions of a record worth scanning, commitment/closing first.
///
/// `get_spendable_outputs` takes one transaction at a time, and LDK's own docs
/// describe walking "all descendant spending transactions starting from the
/// channel's funding transaction and going down three levels" for a full
/// history. We walk only what the record NAMES — the commitment plus one level
/// of descendants it already recorded — because an unbounded descendant crawl
/// would mean recursive Esplora outspend polling of channel outpoints through
/// the first-party backend on every boot, which is the cost the budgeted
/// reconcile pass (`reconcile.ts:56-59`) exists to avoid.
///
/// Two roles are skipped as pure cost:
/// - `sweep`: our own sweep pays the bdk wallet's destination script, so any
///   descriptor it yielded would be a wallet-owned `StaticOutput` that U11
///   excludes anyway.
/// - `anchor_cpfp`: a CPFP child spends the 330-sat anchor and pays change back
///   to the wallet; it resolves no channel balance.
fn replay_candidates(record: &CloseRecord) -> Vec<&CloseRecordTx> {
    let scannable =
        |tx: &&CloseRecordTx| !matches!(tx.role, CloseTxRole::Sweep | CloseTxRole::AnchorCpfp);
    let mut candidates: Vec<&CloseRecordTx> = record
        .txs
        .iter()
        .filter(|tx| tx.role.is_close())
        .filter(scannable)
        .collect();
    candidates.extend(
        record
            .txs
            .iter()
            .filter(|tx| !tx.role.is_close())
            .filter(scannable),
    );
    candidates
}

// ---------------------------------------------------------------------------
// The pass
// ---------------------------------------------------------------------------

/// What one replay pass did — logged, and used by the caller to decide whether
/// to wake an immediate sweep.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct ReplaySummary {
    /// Records that passed [`needs_descriptor_replay`] (the only ones that
    /// cost network).
    pub(crate) records_examined: u32,
    /// Descriptors newly persisted into U11's store (post-dedup,
    /// post-exclusion).
    pub(crate) descriptors_tracked: u32,
    /// Candidates dropped by the already-spent guard.
    pub(crate) already_spent: u32,
}

/// One replay pass. Idempotent: re-running it re-derives the same descriptors
/// and U11's dedup (by descriptor hex AND by outpoint) drops them, so nothing
/// double-tracks. Failure-tolerant per record and per transaction: an Esplora
/// error or a missing monitor skips that unit and leaves the record exactly as
/// it was for the next boot. Never broadcasts — it only enqueues; the sweep
/// engine owns every broadcast decision, including the fee-sanity middleware.
pub(crate) async fn replay_missed_spendable_outputs(
    close_records: &Arc<CloseRecordStore>,
    monitors: &dyn MonitorDescriptors,
    chain: &dyn ChainTruth,
    sweep: &SweepEngine,
    logger: &Arc<Logger>,
) -> ReplaySummary {
    let mut summary = ReplaySummary::default();
    let records: Vec<CloseRecord> = close_records
        .snapshot()
        .into_iter()
        .filter(needs_descriptor_replay)
        .collect();
    if records.is_empty() {
        // Steady state (and every wallet with nothing outstanding): zero
        // Esplora calls, zero monitor locks.
        return summary;
    }

    for record in &records {
        let Some(funding_txo) = record.funding_txo.as_ref() else {
            continue;
        };
        // Cheap in-memory check before any network work: a monitor archived by
        // LDK after full resolution can no longer answer, and that is fine —
        // full resolution means the funds were claimed.
        if !monitors.has_monitor(funding_txo) {
            log_info!(
                logger,
                "Replay: no monitor for funding {}:{} — skipping close record {}",
                funding_txo.txid,
                funding_txo.vout,
                short_id(&record.channel_id)
            );
            continue;
        }
        summary.records_examined += 1;

        for candidate in replay_candidates(record) {
            match replay_one_tx(
                record,
                funding_txo,
                candidate,
                monitors,
                chain,
                sweep,
                logger,
            )
            .await
            {
                Ok((tracked, spent)) => {
                    summary.descriptors_tracked += tracked;
                    summary.already_spent += spent;
                }
                Err(e) => log_error!(
                    logger,
                    "Replay: tx {} of close record {} skipped: {e}",
                    candidate.txid,
                    short_id(&record.channel_id)
                ),
            }
        }
    }

    if summary.descriptors_tracked > 0 {
        log_info!(
            logger,
            "Replay: recovered {} missed spendable output(s) from {} close record(s) \
             ({} already spent on chain)",
            summary.descriptors_tracked,
            summary.records_examined,
            summary.already_spent
        );
    }
    summary
}

/// Steps (b)-(e) for one named transaction. Returns (tracked, already-spent).
async fn replay_one_tx(
    record: &CloseRecord,
    funding_txo: &CloseOutpoint,
    candidate: &CloseRecordTx,
    monitors: &dyn MonitorDescriptors,
    chain: &dyn ChainTruth,
    sweep: &SweepEngine,
    logger: &Arc<Logger>,
) -> Result<(u32, u32), String> {
    // (b) The full transaction — `get_spendable_outputs` scans its outputs, so
    // the txid alone is not enough.
    let Some(tx) = chain.full_tx(&candidate.txid).await? else {
        // Unknown to the backend (pruned, or a txid that never confirmed).
        return Ok((0, 0));
    };
    // The record's write-once height is preferred: it saves a query, and
    // reconcile only ever writes a height it observed confirmed.
    let height = match candidate.confirmed_at_height {
        Some(height) => height,
        None => match chain.tx_confirmed_height(&candidate.txid).await? {
            Some(height) => height,
            // Unconfirmed: `get_spendable_outputs` would filter everything out
            // anyway (it needs max(ANTI_REORG_DELAY, to_self_delay) confs).
            None => return Ok((0, 0)),
        },
    };

    // (c) LDK's own replay entry point.
    let Some(descriptors) = monitors.replay_spendable_outputs(funding_txo, &tx, height) else {
        return Err("monitor disappeared mid-pass".to_string());
    };
    if descriptors.is_empty() {
        return Ok((0, 0));
    }

    // (d) The already-spent guard. Load-bearing: `get_spendable_outputs` is a
    // re-derivation and knows nothing about whether the output still exists,
    // so without this a previously-swept output would be re-tracked and left
    // retrying forever inside U11's all-or-nothing batch — poisoning the
    // sweep of the outputs that DO need it.
    let mut survivors = Vec::with_capacity(descriptors.len());
    let mut already_spent = 0u32;
    for descriptor in descriptors {
        let outpoint = descriptor.spendable_outpoint();
        let txid = outpoint.txid.to_string();
        let vout = u32::from(outpoint.index);
        match chain.outpoint_spent(&txid, vout).await {
            Ok(false) => survivors.push(descriptor),
            Ok(true) => {
                already_spent += 1;
                log_info!(
                    logger,
                    "Replay: {txid}:{vout} already spent on chain — not tracking"
                );
            }
            // An unreachable backend must never read as "unspent": tracking a
            // spent output is the fund-freeze failure mode, while skipping an
            // unspent one just defers to the next boot.
            Err(e) => log_error!(
                logger,
                "Replay: outspend check for {txid}:{vout} failed ({e}) — not tracking"
            ),
        }
    }
    if survivors.is_empty() {
        return Ok((0, already_spent));
    }

    // (e) Into the ONE existing sweep queue, attributed to this channel so
    // KTD-7's sweep-tx attribution lands on this close record and U10's
    // completion gate (`pending_sweep_channels`) holds the record open until
    // the funds actually arrive. `track_spendable_outputs` applies the
    // wallet-owned `StaticOutput` exclusion and its own dedup.
    let tracked = sweep
        .track_spendable_outputs(&survivors, Some(record.channel_id.clone()))
        .map_err(|e| e.to_string())?;
    Ok((tracked as u32, already_spent))
}

fn short_id(channel_id: &str) -> &str {
    &channel_id[..channel_id.len().min(8)]
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Mutex;

    use bitcoin::hashes::Hash as _;
    use bitcoin::{absolute::LockTime, transaction::Version, Amount, ScriptBuf, TxOut, Txid};
    use lightning::chain::transaction::OutPoint as LdkOutPoint;
    use lightning::sign::KeysManager;
    use lightning_persister::fs_store::FilesystemStore;

    use super::*;
    use crate::chain::BroadcastOutcome;
    use crate::close_records::{CloseRecordTx, Initiator};
    use crate::fees::CachedFeeEstimator;
    use crate::keys::{derive_wallet_keys, parse_mnemonic, tests::TEST_MNEMONIC};
    use crate::node::{CoreEvent, EventSink};
    use crate::sweep::{SweepBroadcast, SweepStore};
    use crate::vss::store::BoxFuture;
    use crate::wallet::OnchainWallet;

    /// The real mainnet shapes from the wallet that exposed this bug, so the
    /// fixtures read as the incident rather than as `aaaa…`.
    const FUNDING_TXID: &str = "adf83789566d3b1ed888a25a1fb69cf0495102f1077a8908e0c70c21c00d325a";
    const COMMITMENT_TXID: &str =
        "4572e68e6234800e3cd1a2f72a02512090e55e2aa2ad11c7848a656080d101af";
    const CHANNEL_ID: &str = "5a320dc0210cc7e008897a07f1025149f09cb61f5aa288d81e3b6d568937f8ad";
    const CLOSE_HEIGHT: u32 = 959_133;
    const STUCK_SATS: u64 = 11_500;

    // ---------------- fakes ----------------

    #[derive(Default)]
    struct FakeChain {
        /// txid → the transaction the backend serves.
        txs: HashMap<String, Transaction>,
        /// txid → confirmed height.
        heights: HashMap<String, u32>,
        /// `txid:vout` → spender txid (present = already spent).
        spends: HashMap<String, String>,
        /// Every chain read, so "skipped entirely" is assertable.
        calls: AtomicU32,
        fail_outspends: bool,
    }

    impl FakeChain {
        fn calls(&self) -> u32 {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl ChainTruth for FakeChain {
        fn tip_height(&self) -> BoxFuture<'_, Result<u32, String>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move { Ok(CLOSE_HEIGHT + 720) })
        }

        fn outspend<'a>(
            &'a self,
            txid: &'a str,
            vout: u32,
        ) -> BoxFuture<'a, Result<Option<String>, String>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                if self.fail_outspends {
                    return Err("esplora 500".to_string());
                }
                Ok(self.spends.get(&format!("{txid}:{vout}")).cloned())
            })
        }

        fn tx_confirmed_height<'a>(
            &'a self,
            txid: &'a str,
        ) -> BoxFuture<'a, Result<Option<u32>, String>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move { Ok(self.heights.get(txid).copied()) })
        }

        fn full_tx<'a>(
            &'a self,
            txid: &'a str,
        ) -> BoxFuture<'a, Result<Option<Transaction>, String>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move { Ok(self.txs.get(txid).cloned()) })
        }

        fn outpoint_spent<'a>(
            &'a self,
            txid: &'a str,
            vout: u32,
        ) -> BoxFuture<'a, Result<bool, String>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                if self.fail_outspends {
                    return Err("esplora 500".to_string());
                }
                Ok(self.spends.contains_key(&format!("{txid}:{vout}")))
            })
        }
    }

    /// Stands in for the `ChainMonitor`: one funding outpoint, a canned
    /// descriptor set, and a record of the (tx, height) pairs it was asked
    /// about.
    struct FakeMonitors {
        funding: Option<CloseOutpoint>,
        descriptors: Vec<SpendableOutputDescriptor>,
        asked: Mutex<Vec<(Txid, u32)>>,
    }

    impl FakeMonitors {
        fn new(descriptors: Vec<SpendableOutputDescriptor>) -> Self {
            Self {
                funding: Some(CloseOutpoint {
                    txid: FUNDING_TXID.to_string(),
                    vout: 0,
                }),
                descriptors,
                asked: Mutex::new(Vec::new()),
            }
        }

        fn none() -> Self {
            Self {
                funding: None,
                descriptors: Vec::new(),
                asked: Mutex::new(Vec::new()),
            }
        }
    }

    impl MonitorDescriptors for FakeMonitors {
        fn has_monitor(&self, funding_txo: &CloseOutpoint) -> bool {
            self.funding.as_ref() == Some(funding_txo)
        }

        fn replay_spendable_outputs(
            &self,
            funding_txo: &CloseOutpoint,
            tx: &Transaction,
            confirmation_height: u32,
        ) -> Option<Vec<SpendableOutputDescriptor>> {
            if !self.has_monitor(funding_txo) {
                return None;
            }
            self.asked
                .lock()
                .unwrap()
                .push((tx.compute_txid(), confirmation_height));
            Some(self.descriptors.clone())
        }
    }

    struct NoBroadcast;

    impl SweepBroadcast for NoBroadcast {
        fn persist_pending(&self, _tx: &Transaction) {}

        fn broadcast<'a>(&'a self, _tx: &'a Transaction) -> BoxFuture<'a, BroadcastOutcome> {
            // Nothing in this module may broadcast; a call here is a bug.
            panic!("the replay pass must never broadcast");
        }

        fn tx_known<'a>(&'a self, _txid: &'a Txid) -> BoxFuture<'a, bool> {
            Box::pin(async { false })
        }
    }

    #[derive(Default)]
    struct SilentSink;

    impl EventSink for SilentSink {
        fn emit(&self, _event: CoreEvent) {}
    }

    // ---------------- harness ----------------

    struct Harness {
        _dir: tempfile::TempDir,
        close_records: Arc<CloseRecordStore>,
        sweep_store: Arc<SweepStore>,
        engine: SweepEngine,
        logger: Arc<Logger>,
    }

    fn harness() -> Harness {
        let dir = tempfile::tempdir().unwrap();
        let kv_store = Arc::new(FilesystemStore::new(dir.path().join("store")));
        let logger = Arc::new(Logger);
        let keys = derive_wallet_keys(
            &parse_mnemonic(TEST_MNEMONIC).unwrap(),
            bitcoin::Network::Bitcoin,
        );
        let wallet = Arc::new(
            OnchainWallet::new(
                &keys.descriptor_external,
                &keys.descriptor_internal,
                bitcoin::Network::Bitcoin,
                Arc::clone(&kv_store),
                Arc::clone(&logger),
            )
            .unwrap(),
        );
        let keys_manager = Arc::new(KeysManager::new(&keys.ldk_seed, 0, 0, false));
        let sweep_store = Arc::new(SweepStore::new(Arc::clone(&kv_store), Arc::clone(&logger)));
        let close_records = Arc::new(CloseRecordStore::new(
            Arc::clone(&kv_store),
            Arc::clone(&logger),
        ));
        let engine = SweepEngine::new(
            Arc::clone(&sweep_store),
            keys_manager,
            wallet,
            Arc::new(NoBroadcast) as Arc<dyn SweepBroadcast>,
            Arc::new(CachedFeeEstimator::new()),
            Arc::clone(&close_records),
            Arc::new(|| 0),
            Arc::new(SilentSink) as Arc<dyn EventSink>,
            Arc::clone(&logger),
        );
        Harness {
            _dir: dir,
            close_records,
            sweep_store,
            engine,
            logger,
        }
    }

    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(future)
    }

    fn run(harness: &Harness, monitors: &FakeMonitors, chain: &FakeChain) -> ReplaySummary {
        block_on(replay_missed_spendable_outputs(
            &harness.close_records,
            monitors,
            chain,
            &harness.engine,
            &harness.logger,
        ))
    }

    // ---------------- fixtures ----------------

    /// The restored record from the affected wallet: force close by the
    /// remote, 11,500 sats expected, ONLY the commitment attached — no sweep
    /// tx, no resolution, no completedAt.
    fn stuck_record() -> CloseRecord {
        let mut record = CloseRecord::skeleton(CHANNEL_ID, 1_700_000_000_000);
        record.close_type = CloseType::Force;
        record.initiator = Initiator::Remote;
        record.funding_txo = Some(CloseOutpoint {
            txid: FUNDING_TXID.to_string(),
            vout: 0,
        });
        record.expected_amount_sats = Some(STUCK_SATS);
        record.timelock_blocks = Some(144);
        record.txs.push(CloseRecordTx {
            txid: COMMITMENT_TXID.to_string(),
            role: CloseTxRole::Commitment,
            fee_sats: None,
            confirmed_at_height: Some(CLOSE_HEIGHT),
        });
        record
    }

    /// A commitment-shaped transaction: two 330-sat anchors, the 11,500-sat
    /// output at index 2, and the counterparty's 101,521 at index 3.
    fn commitment_tx() -> Transaction {
        let out = |sats: u64| TxOut {
            value: Amount::from_sat(sats),
            script_pubkey: ScriptBuf::new_p2wpkh(&bitcoin::WPubkeyHash::from_byte_array([7; 20])),
        };
        Transaction {
            version: Version::TWO,
            lock_time: LockTime::ZERO,
            input: Vec::new(),
            output: vec![out(330), out(330), out(STUCK_SATS), out(101_521)],
        }
    }

    /// The descriptor LDK re-derives for commitment output 2. A `StaticOutput`
    /// paying a FOREIGN script: not wallet-owned, so U11's exclusion keeps it
    /// (the exclusion itself is covered by `sweep`'s own suite).
    fn stuck_descriptor() -> SpendableOutputDescriptor {
        SpendableOutputDescriptor::StaticOutput {
            outpoint: LdkOutPoint {
                txid: COMMITMENT_TXID.parse().unwrap(),
                index: 2,
            },
            output: TxOut {
                value: Amount::from_sat(STUCK_SATS),
                script_pubkey: ScriptBuf::new_p2wpkh(&bitcoin::WPubkeyHash::from_byte_array(
                    [0x42; 20],
                )),
            },
            channel_keys_id: None,
        }
    }

    fn chain_with_commitment() -> FakeChain {
        let mut chain = FakeChain::default();
        chain
            .txs
            .insert(COMMITMENT_TXID.to_string(), commitment_tx());
        chain
            .heights
            .insert(COMMITMENT_TXID.to_string(), CLOSE_HEIGHT);
        chain
    }

    // ---------------- tests ----------------

    /// The bug, end to end: an unresolved remote force close whose commitment
    /// still holds an unspent output gets its descriptor back into U11's
    /// store, and the existing pending-sweep surface reports it — no new queue,
    /// no new banner.
    #[test]
    fn unresolved_force_close_tracks_the_missed_descriptor() {
        let harness = harness();
        harness.close_records.upsert(stuck_record());
        let monitors = FakeMonitors::new(vec![stuck_descriptor()]);
        let chain = chain_with_commitment();

        let summary = run(&harness, &monitors, &chain);

        assert_eq!(
            summary,
            ReplaySummary {
                records_examined: 1,
                descriptors_tracked: 1,
                already_spent: 0,
            }
        );
        // LDK was asked about the commitment at its recorded height (no extra
        // height query — the record's write-once height was reused).
        assert_eq!(
            *monitors.asked.lock().unwrap(),
            vec![(commitment_tx().compute_txid(), CLOSE_HEIGHT)]
        );
        let pending = harness.sweep_store.pending_info().expect("pending sweep");
        assert_eq!(pending.entry_count, 1);
        assert_eq!(pending.descriptor_count, 1);
        assert_eq!(pending.pending_sats, STUCK_SATS);
        assert!(!pending.has_unknown_value);
        // Per-channel attribution, so KTD-7 can put the sweep txid on this
        // record and U10's completion gate holds it open.
        assert_eq!(
            harness.sweep_store.pending_channel_ids(),
            [CHANNEL_ID.to_string()].into_iter().collect()
        );
    }

    /// The regression that protects against zombie sweeps: the identical shape
    /// with the outpoint already spent on chain tracks NOTHING. Re-tracking it
    /// would leave an unspendable member in the all-or-nothing batch and fail
    /// every future sweep of the outputs that DO need one.
    #[test]
    fn already_spent_outpoint_is_never_tracked() {
        let harness = harness();
        harness.close_records.upsert(stuck_record());
        let monitors = FakeMonitors::new(vec![stuck_descriptor()]);
        let mut chain = chain_with_commitment();
        chain.spends.insert(
            format!("{COMMITMENT_TXID}:2"),
            "0000000000000000000000000000000000000000000000000000000000000001".to_string(),
        );

        let summary = run(&harness, &monitors, &chain);

        assert_eq!(
            summary,
            ReplaySummary {
                records_examined: 1,
                descriptors_tracked: 0,
                already_spent: 1,
            }
        );
        assert!(harness.sweep_store.pending_info().is_none());
    }

    /// An unreachable backend is not evidence of "unspent" either — the same
    /// fund-safe direction, deferred to the next boot.
    #[test]
    fn outspend_failure_does_not_track() {
        let harness = harness();
        harness.close_records.upsert(stuck_record());
        let monitors = FakeMonitors::new(vec![stuck_descriptor()]);
        let mut chain = chain_with_commitment();
        chain.fail_outspends = true;

        let summary = run(&harness, &monitors, &chain);

        assert_eq!(summary.descriptors_tracked, 0);
        assert!(harness.sweep_store.pending_info().is_none());
    }

    /// A resolved (verified) record is skipped before any network work: the
    /// funds were observed arriving, so there is nothing to replay and nothing
    /// to pay Esplora for.
    #[test]
    fn verified_close_record_costs_zero_chain_lookups() {
        let harness = harness();
        let mut record = stuck_record();
        record.completed_at_ms = Some(1_700_000_100_000);
        record.resolution = Some(Resolution::Verified);
        harness.close_records.upsert(record);
        let monitors = FakeMonitors::new(vec![stuck_descriptor()]);
        let chain = chain_with_commitment();

        let summary = run(&harness, &monitors, &chain);

        assert_eq!(summary, ReplaySummary::default());
        assert_eq!(chain.calls(), 0, "a resolved record must cost no queries");
        assert!(monitors.asked.lock().unwrap().is_empty());
        assert!(harness.sweep_store.pending_info().is_none());
    }

    /// A CONFIRMED sweep tx is the other positive-evidence skip.
    #[test]
    fn confirmed_sweep_tx_costs_zero_chain_lookups() {
        let harness = harness();
        let mut record = stuck_record();
        record.txs.push(CloseRecordTx {
            txid: "5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a".to_string(),
            role: CloseTxRole::Sweep,
            fee_sats: None,
            confirmed_at_height: Some(CLOSE_HEIGHT + 150),
        });
        harness.close_records.upsert(record);
        let monitors = FakeMonitors::new(vec![stuck_descriptor()]);
        let chain = chain_with_commitment();

        let summary = run(&harness, &monitors, &chain);

        assert_eq!(summary, ReplaySummary::default());
        assert_eq!(chain.calls(), 0);
    }

    /// Idempotency: the pass runs on EVERY boot, and U11's dedup (descriptor
    /// hex + outpoint) means the second run tracks nothing new.
    #[test]
    fn rerunning_tracks_the_descriptor_exactly_once() {
        let harness = harness();
        harness.close_records.upsert(stuck_record());
        let monitors = FakeMonitors::new(vec![stuck_descriptor()]);
        let chain = chain_with_commitment();

        let first = run(&harness, &monitors, &chain);
        let second = run(&harness, &monitors, &chain);

        assert_eq!(first.descriptors_tracked, 1);
        assert_eq!(second.descriptors_tracked, 0);
        let pending = harness.sweep_store.pending_info().expect("pending sweep");
        assert_eq!(pending.entry_count, 1);
        assert_eq!(pending.descriptor_count, 1);
        assert_eq!(pending.pending_sats, STUCK_SATS);
    }

    /// A wallet with no close records does nothing at all — the steady state
    /// this pass must not tax.
    #[test]
    fn no_close_records_does_nothing() {
        let harness = harness();
        let monitors = FakeMonitors::new(vec![stuck_descriptor()]);
        let chain = chain_with_commitment();

        let summary = run(&harness, &monitors, &chain);

        assert_eq!(summary, ReplaySummary::default());
        assert_eq!(chain.calls(), 0);
        assert!(harness.sweep_store.pending_info().is_none());
    }

    /// No loaded monitor (archived after full resolution, or a record for a
    /// channel this boot never loaded) → no network work.
    #[test]
    fn missing_monitor_costs_zero_chain_lookups() {
        let harness = harness();
        harness.close_records.upsert(stuck_record());
        let monitors = FakeMonitors::none();
        let chain = chain_with_commitment();

        let summary = run(&harness, &monitors, &chain);

        assert_eq!(summary, ReplaySummary::default());
        assert_eq!(chain.calls(), 0);
    }

    /// A record with no height on its commitment (restored from another client
    /// before any reconcile pass ran) still recovers on THIS boot — the height
    /// comes from chain truth, so the fix never needs a second launch.
    #[test]
    fn missing_recorded_height_is_resolved_from_chain_truth() {
        let harness = harness();
        let mut record = stuck_record();
        record.txs[0].confirmed_at_height = None;
        harness.close_records.upsert(record);
        let monitors = FakeMonitors::new(vec![stuck_descriptor()]);
        let chain = chain_with_commitment();

        let summary = run(&harness, &monitors, &chain);

        assert_eq!(summary.descriptors_tracked, 1);
        assert_eq!(
            *monitors.asked.lock().unwrap(),
            vec![(commitment_tx().compute_txid(), CLOSE_HEIGHT)]
        );
    }

    /// Coop closes, zero/absent expected amounts, and records with no funding
    /// outpoint are all outside the predicate: they can hold no unswept
    /// force-close descriptor, so they must not cost a query.
    #[test]
    fn ineligible_records_are_filtered_by_the_predicate() {
        let coop = {
            let mut record = stuck_record();
            record.close_type = CloseType::Coop;
            record
        };
        let zero_balance = {
            let mut record = stuck_record();
            record.expected_amount_sats = Some(0);
            record
        };
        let unknown_balance = {
            let mut record = stuck_record();
            record.expected_amount_sats = None;
            record
        };
        let no_funding = {
            let mut record = stuck_record();
            record.funding_txo = None;
            record
        };
        let no_close_tx = {
            let mut record = stuck_record();
            record.txs.clear();
            record
        };
        for record in [coop, zero_balance, unknown_balance, no_funding, no_close_tx] {
            assert!(
                !needs_descriptor_replay(&record),
                "expected {record:?} to be filtered out"
            );
        }
        assert!(needs_descriptor_replay(&stuck_record()));
        // `resolved_unverified` is NOT positive evidence — the record resolved
        // on chain but our wallet never saw the funds, which is exactly this
        // bug's shape. Re-examined, and the already-spent guard makes it safe.
        let mut unverified = stuck_record();
        unverified.completed_at_ms = Some(1_700_000_100_000);
        unverified.resolution = Some(Resolution::Unverified);
        assert!(needs_descriptor_replay(&unverified));
    }
}
