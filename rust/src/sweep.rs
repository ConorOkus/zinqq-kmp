//! Core-owned sweep pipeline (U11; R8; KTD-8, KTD-9) — the ported PWA
//! descriptor store, replacing the spike's `OutputSweeperSync` wiring.
//!
//! `OutputSweeper` was rejected after API review (KTD-8): it exposes no
//! untrack/release, its regenerate-and-rebroadcast cycle would race a
//! parallel subsidized transaction over the same outpoints, and it emits no
//! per-tx attribution — which close-record status derivation (U10) requires.
//! Instead this module owns:
//!
//! - a KVStore-persisted spendable-outputs store in the PWA's entry shape
//!   (`sweep.ts:27-31`: serialized descriptors + source channel + outpoints
//!   with values), UUID keys under `("spendable_outputs", "")`;
//! - the pre-persist wallet-owned `StaticOutput` exclusion
//!   (`sweep.ts:117-149`), including post-recovery re-derivation by
//!   `channel_keys_id` through U1's deterministic destination-index scheme —
//!   one such descriptor in the all-or-nothing batch makes every sweep fail
//!   (`KeysManager` categorically cannot sign it), freezing the descriptors
//!   that DO need sweeping: a real historic fund-freeze;
//! - dedup by descriptor hex + outpoint (`sweep.ts:174,264-271`): LDK
//!   replays `SpendableOutputs` events across restarts while a sweep keeps
//!   failing;
//! - the all-or-nothing `spend_spendable_outputs` batch at a 6-block rate
//!   clamped 2–500 sat/vB (`sweep.ts:18-20,367-398`), with
//!   structural-vs-conditional failure classification: a member that fails a
//!   fee-independent signing probe is structurally unsignable and is removed
//!   so the batch can retry; fee/dust failures are conditional and fall
//!   through to the subsidized path;
//! - the fee-subsidized fallback (`subsidized-sweep.ts`): LDK PSBT at the
//!   250 sat/kW floor + confirmed wallet P2WPKH inputs as fee subsidy —
//!   net-positive gated, ≤ 20 largest-first inputs, reserve-aware (U8
//!   arithmetic), 546-sat dust gate, RBF sequence, changeless variant,
//!   dual-signed (LDK first, then bdk with `trust_witness_utxo`),
//!   independently fee-verified to the sat BEFORE broadcast, with a
//!   session-scoped subsidy-outpoint reservation and
//!   `apply_unconfirmed_txs` visibility;
//! - sentinel-aware broadcast verification: descriptors are deleted only
//!   after the broadcast outcome is chain truth — a clean accept, or (for
//!   shared-input subsidized txs) a success sentinel VERIFIED against the
//!   chain view, because a concurrently spent wallet input produces the same
//!   "-25" error and trusting it would delete descriptors while the funds
//!   never moved (`subsidized-sweep.ts:426-437`);
//! - the fee-sanity middleware (chain.rs, adopted from the incident review):
//!   no sweep/subsidy broadcast may exceed 5x a fresh 3-block estimate;
//! - pending-sweep state with lower-bound semantics
//!   (`sweep.ts:62-82,164-212`) and `SweepStateChanged` events;
//! - per-channel sweep-tx attribution feeding close records
//!   (`close_records::record_sweep_tx`, KTD-7/U10).

use std::collections::HashSet;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use bitcoin::secp256k1::{All, Secp256k1};
use bitcoin::{
    Amount, OutPoint, Psbt, ScriptBuf, Sequence, Transaction, TxIn, TxOut, Txid, Witness,
};
use lightning::log_error;
use lightning::log_info;
use lightning::sign::{KeysManager, OutputSpender as _, SpendableOutputDescriptor};
use lightning::util::logger::Logger as _;
use lightning::util::persist::KVStoreSync;
use lightning::util::ser::{Readable, Writeable as _};
use lightning_persister::fs_store::FilesystemStore;
use serde::{Deserialize, Serialize};

use crate::chain::{check_fee_sanity, fee_sanity_max_sat_per_kw, BroadcastOutcome};
use crate::close_records::{record_sweep_tx, CloseRecordStore};
use crate::fees::CachedFeeEstimator;
use crate::node::{CoreEvent, EventSink};
use crate::recovery::RecoverySweeper;
use crate::signer::destination_index;
use crate::types::Logger;
use crate::util::hex_str;
use crate::vss::store::BoxFuture;
use crate::wallet::OnchainWallet;

/// KVStore namespace for spendable-outputs entries (the PWA's
/// `ldk_spendable_outputs` IDB store), keyed by UUID.
pub(crate) const SPENDABLE_OUTPUTS_PRIMARY_NAMESPACE: &str = "spendable_outputs";
pub(crate) const SPENDABLE_OUTPUTS_SECONDARY_NAMESPACE: &str = "";

/// Sweep fee clamp (PWA `sweep.ts:19-20`): the 6-block estimate is ceil'd
/// then clamped into [2, 500] sat/vB before the x250 sat/kW conversion.
pub(crate) const MIN_SWEEP_RATE_SAT_PER_VB: u64 = 2;
pub(crate) const MAX_SWEEP_RATE_SAT_PER_VB: u64 = 500;

/// The subsidized path's LDK-side floor feerate (PWA
/// `subsidized-sweep.ts:44`): nearly all swept value survives as the
/// destination output; wallet inputs bring the tx up to the target rate.
pub(crate) const FLOOR_FEERATE_SAT_PER_KW: u32 = 250;
/// Relay dust gate applied to every output (PWA `subsidized-sweep.ts:45`).
pub(crate) const DUST_LIMIT_SATS: u64 = 546;
/// P2WPKH subsidy input: 41 vbytes base (164 wu) + ~108 wu witness.
pub(crate) const SUBSIDY_INPUT_WEIGHT_WU: u64 = 272;
/// P2WPKH change TxOut: 31 bytes.
pub(crate) const CHANGE_OUTPUT_WEIGHT_WU: u64 = 124;
/// Standardness headroom: never build a monster subsidy tx.
pub(crate) const MAX_SUBSIDY_INPUTS: usize = 20;
/// Wallet subsidy inputs signal RBF (PWA `psbt-surgery` sequence).
pub(crate) const RBF_SEQUENCE: Sequence = Sequence(0xFFFF_FFFD);

/// Sweep retry cadence (U11, PWA `context.tsx:1495-1563`): the tick fires
/// every 60 s; a pass runs every tick while shortfall-blocked (a fresh
/// deposit should be picked up promptly) and every
/// [`SWEEP_RETRY_EVERY_TICKS`]th tick (~hourly) otherwise — fee conditions
/// change slowly, and event/startup sweeps still fire immediately.
pub(crate) const SWEEP_TICK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);
pub(crate) const SWEEP_RETRY_EVERY_TICKS: u32 = 60;

// ---------------------------------------------------------------------------
// Typed errors
// ---------------------------------------------------------------------------

/// Typed sweep-store failures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SweepError {
    /// A spendable-outputs entry failed to serialize or write. The caller
    /// must have LDK REPLAY the event rather than drop funds.
    Persist { detail: String },
}

impl fmt::Display for SweepError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SweepError::Persist { detail } => {
                write!(f, "failed to persist spendable outputs: {detail}")
            }
        }
    }
}

impl std::error::Error for SweepError {}

// ---------------------------------------------------------------------------
// Persisted entry shape (PWA sweep.ts:27-31)
// ---------------------------------------------------------------------------

/// One tracked outpoint's display facts (PWA shape; `value_sats` as a string
/// mirrors the PWA's persisted bigint and keeps unreadable values non-fatal).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StoredOutpoint {
    pub(crate) txid: String,
    pub(crate) vout: u32,
    pub(crate) value_sats: String,
}

/// One persisted `SpendableOutputs` event (post-exclusion): the PWA's
/// `SpendableOutputsEntry` with descriptors as hex (KVStore is byte-valued;
/// no legacy bare-array shape exists on native — the store is new here).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StoredEntry {
    /// Serialized `SpendableOutputDescriptor`s, hex-encoded.
    pub(crate) descriptors: Vec<String>,
    /// Source channel (per-channel sweep attribution, U10/KTD-7); `None`
    /// when LDK reported none.
    pub(crate) channel_id_hex: Option<String>,
    pub(crate) outpoints: Vec<StoredOutpoint>,
}

// ---------------------------------------------------------------------------
// Pending-sweep state (PWA sweep.ts:62-98,164-212)
// ---------------------------------------------------------------------------

/// Snapshot of outputs still waiting to sweep, for user-facing surfaces.
/// `pending_sats` is a LOWER BOUND: entries with unreadable value data set
/// `has_unknown_value` instead of gating the banner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingSweepInfo {
    pub entry_count: u32,
    pub descriptor_count: u32,
    /// Total KNOWN value across pending outputs (sats) — a lower bound.
    pub pending_sats: u64,
    /// True when at least one entry carries unreadable value data, so
    /// `pending_sats` undercounts the real total.
    pub has_unknown_value: bool,
    /// True when the most recent sweep attempt failed (dust, fees,
    /// broadcast, undecodable member).
    pub last_attempt_failed: bool,
    /// True when a subsidized sweep would rescue the funds but the confirmed
    /// on-chain balance can't cover the subsidy — adding funds unblocks it.
    pub needs_onchain_funds: bool,
    /// Estimated additional confirmed sats needed; `None` when not in
    /// shortfall.
    pub shortfall_sats: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct SweepFlags {
    last_attempt_failed: bool,
    shortfall_sats: Option<u64>,
}

// ---------------------------------------------------------------------------
// Store
// ---------------------------------------------------------------------------

/// The KVStore-persisted descriptor store plus the session sweep-attempt
/// flags (the PWA's module-level `lastAttemptFailed` /
/// `onchainShortfallSats`). Owned by the `Node` (readable while stopped,
/// like the payment store); the running [`SweepEngine`] shares it.
pub(crate) struct SweepStore {
    kv_store: Arc<FilesystemStore>,
    flags: Mutex<SweepFlags>,
    logger: Arc<Logger>,
}

impl SweepStore {
    pub(crate) fn new(kv_store: Arc<FilesystemStore>, logger: Arc<Logger>) -> Self {
        Self {
            kv_store,
            flags: Mutex::new(SweepFlags::default()),
            logger,
        }
    }

    /// Every persisted entry. Corrupt entries are logged and skipped — they
    /// hold nothing decodable to sweep; visibility degrades, funds do not.
    pub(crate) fn entries(&self) -> Vec<(String, StoredEntry)> {
        let keys = match self.kv_store.list(
            SPENDABLE_OUTPUTS_PRIMARY_NAMESPACE,
            SPENDABLE_OUTPUTS_SECONDARY_NAMESPACE,
        ) {
            Ok(keys) => keys,
            Err(e) => {
                log_error!(self.logger, "Failed to list spendable outputs: {e}");
                return Vec::new();
            }
        };
        let mut entries = Vec::new();
        for key in keys {
            match self.kv_store.read(
                SPENDABLE_OUTPUTS_PRIMARY_NAMESPACE,
                SPENDABLE_OUTPUTS_SECONDARY_NAMESPACE,
                &key,
            ) {
                Ok(bytes) => match serde_json::from_slice::<StoredEntry>(&bytes) {
                    Ok(entry) => entries.push((key, entry)),
                    Err(e) => {
                        log_error!(self.logger, "Corrupt spendable-outputs entry {key}: {e}")
                    }
                },
                Err(e) => log_error!(self.logger, "Failed to read spendable outputs {key}: {e}"),
            }
        }
        entries
    }

    /// Persists (or rewrites) one entry.
    pub(crate) fn write_entry(&self, key: &str, entry: &StoredEntry) -> Result<(), SweepError> {
        let bytes = serde_json::to_vec(entry).map_err(|e| SweepError::Persist {
            detail: format!("serialize: {e}"),
        })?;
        self.kv_store
            .write(
                SPENDABLE_OUTPUTS_PRIMARY_NAMESPACE,
                SPENDABLE_OUTPUTS_SECONDARY_NAMESPACE,
                key,
                bytes,
            )
            .map_err(|e| SweepError::Persist {
                detail: e.to_string(),
            })
    }

    /// Removes entries after their outputs are swept (or emptied by pruning).
    pub(crate) fn remove_entries(&self, keys: &[String]) {
        for key in keys {
            if let Err(e) = self.kv_store.remove(
                SPENDABLE_OUTPUTS_PRIMARY_NAMESPACE,
                SPENDABLE_OUTPUTS_SECONDARY_NAMESPACE,
                key,
                false,
            ) {
                log_error!(self.logger, "Failed to remove swept entry {key}: {e}");
            }
        }
    }

    /// The channel ids with un-swept outputs pending — these BLOCK
    /// close-record completion (U10's `pending_sweep_channels` seam: a
    /// partial sweep's receipt must not complete the record early).
    pub(crate) fn pending_channel_ids(&self) -> HashSet<String> {
        self.entries()
            .into_iter()
            .filter_map(|(_, entry)| entry.channel_id_hex)
            .collect()
    }

    /// What's still waiting to sweep, deduped exactly like the PWA
    /// (`sweep.ts:164-212`): replayed events can persist the same output
    /// under multiple keys until a sweep pass prunes them — the banner must
    /// not double-count. `None` when nothing is pending.
    pub(crate) fn pending_info(&self) -> Option<PendingSweepInfo> {
        let entries = self.entries();
        if entries.is_empty() {
            return None;
        }
        let mut descriptor_count = 0u32;
        let mut pending_sats = 0u64;
        let mut has_unknown_value = false;
        let mut seen_descriptor_hex = HashSet::new();
        let mut seen_outpoints = HashSet::new();
        for (_, entry) in &entries {
            for hex in &entry.descriptors {
                if seen_descriptor_hex.insert(hex.clone()) {
                    descriptor_count += 1;
                }
            }
            if entry.outpoints.is_empty() {
                has_unknown_value = true;
            }
            for outpoint in &entry.outpoints {
                if !seen_outpoints.insert(format!("{}:{}", outpoint.txid, outpoint.vout)) {
                    continue;
                }
                match outpoint.value_sats.parse::<u64>() {
                    Ok(value) => pending_sats += value,
                    // Unreadable value data must never gate the sweep or the
                    // banner — lower-bound semantics.
                    Err(_) => has_unknown_value = true,
                }
            }
        }
        let flags = *self.flags.lock().unwrap();
        Some(PendingSweepInfo {
            entry_count: entries.len() as u32,
            descriptor_count,
            pending_sats,
            has_unknown_value,
            last_attempt_failed: flags.last_attempt_failed,
            needs_onchain_funds: flags.shortfall_sats.is_some(),
            shortfall_sats: flags.shortfall_sats,
        })
    }

    /// Whether only incoming on-chain funds block the sweep — gates the
    /// faster 60 s retry cadence (PWA `sweepNeedsOnchainFunds`).
    pub(crate) fn needs_onchain_funds(&self) -> bool {
        self.flags.lock().unwrap().shortfall_sats.is_some()
    }

    /// U4 restore: drop the replaced wallet's session flags (the entries
    /// themselves lived in the replaced store directory).
    pub(crate) fn reset(&self) {
        *self.flags.lock().unwrap() = SweepFlags::default();
    }

    /// Records an attempt outcome; returns whether the flags changed (the
    /// `SweepStateChanged` trigger).
    fn set_flags(&self, last_attempt_failed: bool, shortfall_sats: Option<u64>) -> bool {
        let mut flags = self.flags.lock().unwrap();
        let next = SweepFlags {
            last_attempt_failed,
            shortfall_sats,
        };
        let changed = *flags != next;
        *flags = next;
        changed
    }
}

// ---------------------------------------------------------------------------
// Broadcast seam (sentinel-aware verification)
// ---------------------------------------------------------------------------

/// The sweep engine's broadcast surface — [`crate::chain::ChainSource`] in
/// production, a mock in tests (offline descriptor-deletion coverage).
pub(crate) trait SweepBroadcast: Send + Sync {
    /// Persists the tx to the pending-broadcast store BEFORE the attempt
    /// (U12/KTD-9 crash safety).
    fn persist_pending(&self, tx: &Transaction);
    fn broadcast<'a>(&'a self, tx: &'a Transaction) -> BoxFuture<'a, BroadcastOutcome>;
    /// Whether the chain view knows `txid` (mempool or confirmed);
    /// unreachable reads as `false` — never proof.
    fn tx_known<'a>(&'a self, txid: &'a Txid) -> BoxFuture<'a, bool>;
}

impl SweepBroadcast for crate::chain::ChainSource {
    fn persist_pending(&self, tx: &Transaction) {
        self.persist_pending_broadcast(tx);
    }

    fn broadcast<'a>(&'a self, tx: &'a Transaction) -> BoxFuture<'a, BroadcastOutcome> {
        Box::pin(self.broadcast_transaction(tx))
    }

    fn tx_known<'a>(&'a self, txid: &'a Txid) -> BoxFuture<'a, bool> {
        Box::pin(self.tx_known_to_chain(txid))
    }
}

// ---------------------------------------------------------------------------
// Subsidized-sweep math (pure — PWA subsidized-sweep.ts:89-164)
// ---------------------------------------------------------------------------

/// A confirmed wallet P2WPKH UTXO offered as fee subsidy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SubsidyInput {
    pub(crate) outpoint: OutPoint,
    pub(crate) value_sats: u64,
    pub(crate) script_pubkey: ScriptBuf,
}

/// A successful subsidy selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SubsidySelection {
    pub(crate) selected: Vec<SubsidyInput>,
    /// `None` → changeless variant (remainder absorbed into fee, bounded by
    /// dust + the change-output fee).
    pub(crate) change_sats: Option<u64>,
    pub(crate) total_fee_sats: u64,
    /// What the wallet actually contributes to the fee: Σ selected − change.
    pub(crate) subsidy_sats: u64,
}

/// Selection outcome: a plan, or the shortfall that blocks it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SelectionOutcome {
    Selected(SubsidySelection),
    Shortfall {
        needed_subsidy_sats: u64,
        available_sats: u64,
    },
}

/// `feeForWeight` (`subsidized-sweep.ts:89-92`): ceil to vbytes, then rate.
pub(crate) fn fee_for_weight(weight_wu: u64, rate_sat_per_vb: u64) -> u64 {
    weight_wu.div_ceil(4) * rate_sat_per_vb
}

/// Largest-first selection of fee-subsidy inputs (PWA
/// `selectSubsidyInputs`, `subsidized-sweep.ts:104-164`). `candidates` must
/// already be sorted largest-first. The reserve constrains the FINAL
/// subsidy, not individual UTXOs — change returns to the wallet, so what
/// leaves the balance is exactly `subsidy_sats`.
pub(crate) fn select_subsidy_inputs(
    candidates: &[SubsidyInput],
    ldk_weight_wu: u64,
    ldk_fee_sats: u64,
    target_rate_sat_per_vb: u64,
    reserve_sats: u64,
) -> SelectionOutcome {
    let total_available: u64 = candidates.iter().map(|c| c.value_sats).sum();
    let spendable = total_available.saturating_sub(reserve_sats);

    let needed_with_change = |n: u64| {
        fee_for_weight(
            ldk_weight_wu + n * SUBSIDY_INPUT_WEIGHT_WU + CHANGE_OUTPUT_WEIGHT_WU,
            target_rate_sat_per_vb,
        )
        .saturating_sub(ldk_fee_sats)
    };
    let needed_changeless = |n: u64| {
        fee_for_weight(
            ldk_weight_wu + n * SUBSIDY_INPUT_WEIGHT_WU,
            target_rate_sat_per_vb,
        )
        .saturating_sub(ldk_fee_sats)
    };

    let mut selected: Vec<SubsidyInput> = Vec::new();
    let mut selected_sum = 0u64;

    for utxo in candidates {
        if selected.len() >= MAX_SUBSIDY_INPUTS {
            break;
        }
        selected.push(utxo.clone());
        selected_sum += utxo.value_sats;
        let n = selected.len() as u64;

        let needed = needed_with_change(n);
        if needed <= spendable && selected_sum >= needed {
            let change = selected_sum - needed;
            if change >= DUST_LIMIT_SATS {
                return SelectionOutcome::Selected(SubsidySelection {
                    selected,
                    change_sats: Some(change),
                    total_fee_sats: ldk_fee_sats + needed,
                    subsidy_sats: needed,
                });
            }
        }

        // The changeless variant can still fit when the with-change subsidy
        // exceeds the spendable budget — it must be tried before giving up.
        let needed_drained = needed_changeless(n);
        if selected_sum >= needed_drained && selected_sum <= spendable {
            return SelectionOutcome::Selected(SubsidySelection {
                selected,
                change_sats: None,
                total_fee_sats: ldk_fee_sats + selected_sum,
                subsidy_sats: selected_sum,
            });
        }

        // Once both the fee requirement and the selection itself exceed the
        // spendable budget, adding inputs only raises both — no solution.
        if needed > spendable && selected_sum > spendable {
            break;
        }
    }

    let n = (selected.len().max(1)) as u64;
    SelectionOutcome::Shortfall {
        needed_subsidy_sats: needed_with_change(n),
        available_sats: spendable,
    }
}

/// Subsidized-sweep outcomes (PWA `SubsidizedSweepOutcome`,
/// `subsidized-sweep.ts:53-57`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SubsidizedOutcome {
    Broadcast {
        txid: Txid,
        subsidy_sats: u64,
    },
    Shortfall {
        needed_subsidy_sats: u64,
        available_sats: u64,
        shortfall_sats: u64,
    },
    NotEconomical {
        needed_subsidy_sats: u64,
        pending_sats: u64,
    },
    Failed {
        reason: String,
    },
}

// ---------------------------------------------------------------------------
// Engine
// ---------------------------------------------------------------------------

/// One sweep pass's summary; `swept` is the count of descriptors swept (the
/// PWA's `SweepResult.swept`, which the auto-recover seam gates on).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct SweepPassSummary {
    pub(crate) swept: u32,
    pub(crate) skipped: u32,
    pub(crate) txid: Option<Txid>,
}

/// One decoded batch member with everything needed to prune it back out.
struct BatchMember {
    bytes: Vec<u8>,
    descriptor: SpendableOutputDescriptor,
    entry_key: String,
}

/// The running sweep engine: tracks `SpendableOutputs` events into the
/// store and runs sweep passes (startup, event-triggered, periodic retry).
pub(crate) struct SweepEngine {
    store: Arc<SweepStore>,
    keys_manager: Arc<KeysManager>,
    wallet: Arc<OnchainWallet>,
    broadcast: Arc<dyn SweepBroadcast>,
    fee_estimator: Arc<CachedFeeEstimator>,
    close_records: Arc<CloseRecordStore>,
    /// Confirmed sats to leave untouched for anchor CPFP (0 when no channels
    /// are open) — U8's reserve arithmetic, read at sweep time.
    reserve_sats: Arc<dyn Fn() -> u64 + Send + Sync>,
    event_sink: Arc<dyn EventSink>,
    logger: Arc<Logger>,
    /// Outpoints consumed by a subsidized broadcast this session: the bdk
    /// wallet only learns of the spend at the next chain sync, so without
    /// this a second sweep in that window could re-select the same UTXO and
    /// RBF-replace the first tx AFTER its descriptors were deleted —
    /// permanent fund loss (PWA `subsidized-sweep.ts:166-175`). Never
    /// removed within a session.
    spent_subsidy_outpoints: Mutex<HashSet<OutPoint>>,
    /// Only one sweep runs at a time (PWA `sweepInProgress`).
    in_progress: AtomicBool,
    secp: Secp256k1<All>,
}

impl SweepEngine {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        store: Arc<SweepStore>,
        keys_manager: Arc<KeysManager>,
        wallet: Arc<OnchainWallet>,
        broadcast: Arc<dyn SweepBroadcast>,
        fee_estimator: Arc<CachedFeeEstimator>,
        close_records: Arc<CloseRecordStore>,
        reserve_sats: Arc<dyn Fn() -> u64 + Send + Sync>,
        event_sink: Arc<dyn EventSink>,
        logger: Arc<Logger>,
    ) -> Self {
        Self {
            store,
            keys_manager,
            wallet,
            broadcast,
            fee_estimator,
            close_records,
            reserve_sats,
            event_sink,
            logger,
            spent_subsidy_outpoints: Mutex::new(HashSet::new()),
            in_progress: AtomicBool::new(false),
            secp: Secp256k1::new(),
        }
    }

    /// Whether a `StaticOutput` pays a script the bdk wallet already owns
    /// (PWA `isWalletOwnedStaticOutput`, `sweep.ts:117-149`): U1's signer
    /// hands LDK wallet-derived destination scripts, so force-close
    /// resolutions produce `StaticOutput`s whose funds are already in the
    /// wallet — and which `KeysManager` categorically cannot sign. One such
    /// descriptor in the all-or-nothing batch fails every sweep.
    ///
    /// After a cross-device recovery the destination index may not be
    /// revealed yet, making the ownership check false for a script the
    /// wallet can in fact spend — so a pure re-derivation by
    /// `channel_keys_id` (U1's deterministic index scheme) is compared
    /// first, and the reveal (a wallet mutation) happens only on a
    /// confirmed match. On doubt the descriptor is KEPT — the fund-safe
    /// direction.
    pub(crate) fn is_wallet_owned_static_output(
        &self,
        descriptor: &SpendableOutputDescriptor,
    ) -> bool {
        let SpendableOutputDescriptor::StaticOutput {
            output,
            channel_keys_id,
            ..
        } = descriptor
        else {
            return false;
        };
        if self.wallet.is_mine_script(&output.script_pubkey) {
            return true;
        }
        if let Some(keys_id) = channel_keys_id {
            if keys_id.iter().any(|byte| *byte != 0) {
                let index = destination_index(keys_id);
                if self.wallet.peek_external_script(index) == output.script_pubkey {
                    // Reveal so bdk tracks the address and the funds show in
                    // the balance; failure only delays visibility.
                    let _ = self.wallet.destination_script_for_index(index);
                    return true;
                }
            }
        }
        false
    }

    /// Persists a `SpendableOutputs` event (the node's event arm, U11):
    /// wallet-owned `StaticOutput`s are excluded BEFORE persist and
    /// duplicates of already-tracked descriptors/outpoints are dropped
    /// (event replay). Returns the number of descriptors persisted; the
    /// caller replays the event on `Err` rather than dropping funds.
    pub(crate) fn track_spendable_outputs(
        &self,
        outputs: &[SpendableOutputDescriptor],
        channel_id_hex: Option<String>,
    ) -> Result<usize, SweepError> {
        let mut known_descriptor_hex = HashSet::new();
        let mut known_outpoints = HashSet::new();
        for (_, entry) in self.store.entries() {
            known_descriptor_hex.extend(entry.descriptors);
            known_outpoints.extend(
                entry
                    .outpoints
                    .iter()
                    .map(|o| format!("{}:{}", o.txid, o.vout)),
            );
        }

        let mut descriptors = Vec::new();
        let mut outpoints = Vec::new();
        let mut excluded = 0usize;
        for descriptor in outputs {
            if self.is_wallet_owned_static_output(descriptor) {
                excluded += 1;
                continue;
            }
            let hex = hex_str(&descriptor.encode());
            let outpoint = descriptor.spendable_outpoint();
            let outpoint_key = format!("{}:{}", outpoint.txid, outpoint.index);
            if known_descriptor_hex.contains(&hex) || known_outpoints.contains(&outpoint_key) {
                // Replayed event: the output is already tracked under an
                // earlier key; a duplicate would fail both LDK spend paths.
                continue;
            }
            known_descriptor_hex.insert(hex.clone());
            known_outpoints.insert(outpoint_key);
            outpoints.push(StoredOutpoint {
                txid: outpoint.txid.to_string(),
                vout: u32::from(outpoint.index),
                value_sats: descriptor_value_sats(descriptor).to_string(),
            });
            descriptors.push(hex);
        }
        if excluded > 0 {
            log_info!(
                self.logger,
                "{excluded} spendable output(s) already pay the on-chain wallet; excluded"
            );
        }
        if descriptors.is_empty() {
            return Ok(0);
        }

        let count = descriptors.len();
        let key = new_entry_key(&self.keys_manager);
        self.store.write_entry(
            &key,
            &StoredEntry {
                descriptors,
                channel_id_hex,
                outpoints,
            },
        )?;
        log_info!(self.logger, "Tracked {count} spendable output(s) as {key}");
        Ok(count)
    }

    /// One full sweep pass (PWA `sweepSpendableOutputs`, `sweep.ts:233-470`):
    /// prune → all-or-nothing spend → classify failures → subsidized
    /// fallback → sentinel-aware broadcast → delete + attribute. Guarded
    /// against concurrent execution.
    pub(crate) async fn sweep_once(&self) -> SweepPassSummary {
        if self.in_progress.swap(true, Ordering::AcqRel) {
            return SweepPassSummary::default();
        }
        let summary = self.sweep_pass().await;
        self.in_progress.store(false, Ordering::Release);
        summary
    }

    async fn sweep_pass(&self) -> SweepPassSummary {
        let entries = self.store.entries();
        if entries.is_empty() {
            return SweepPassSummary::default();
        }

        // ---- Stage 1: decode, prune (wallet-owned + duplicates), rewrite.
        let mut batch: Vec<BatchMember> = Vec::new();
        let mut channels: HashSet<String> = HashSet::new();
        let mut batch_keys: Vec<String> = Vec::new();
        let mut emptied_keys: Vec<String> = Vec::new();
        let mut seen_descriptor_hex: HashSet<String> = HashSet::new();
        let mut skipped = 0u32;

        for (key, entry) in entries {
            let mut kept_hexes = Vec::new();
            let mut members = Vec::new();
            let mut pruned_outpoint_keys = HashSet::new();
            let mut pruned = 0usize;
            let mut valid = true;

            for hex in &entry.descriptors {
                // A replayed event can persist the same descriptor under two
                // keys; duplicates make both LDK spend paths fail outright.
                if seen_descriptor_hex.contains(hex) {
                    pruned += 1;
                    continue;
                }
                let Some(bytes) = decode_hex(hex) else {
                    log_error!(self.logger, "Undecodable descriptor hex in entry {key}");
                    valid = false;
                    break;
                };
                let Ok(descriptor) =
                    SpendableOutputDescriptor::read(&mut std::io::Cursor::new(&bytes))
                else {
                    log_error!(
                        self.logger,
                        "Failed to deserialize a spendable-output descriptor in entry {key}"
                    );
                    valid = false;
                    break;
                };
                seen_descriptor_hex.insert(hex.clone());
                // Wallet-owned StaticOutputs need no sweep and would poison
                // the batch (a pre-exclusion store may still hold one, e.g.
                // an entry written before a recovery re-derivation matched).
                if self.is_wallet_owned_static_output(&descriptor) {
                    let outpoint = descriptor.spendable_outpoint();
                    pruned_outpoint_keys.insert(format!(
                        "{}:{}",
                        outpoint.txid,
                        u32::from(outpoint.index)
                    ));
                    pruned += 1;
                    continue;
                }
                kept_hexes.push(hex.clone());
                members.push((bytes, descriptor));
            }

            if !valid {
                // Undecodable entries are stuck funds: kept, counted, and
                // surfaced via last_attempt_failed (PWA sweep.ts:301-304).
                skipped += entry.descriptors.len() as u32;
                continue;
            }
            if members.is_empty() {
                emptied_keys.push(key);
                continue;
            }
            if pruned > 0 {
                // Persist the pruned entry so dropped descriptors never
                // re-enter the batch or the banner, even if this attempt
                // fails. A failed write must not abort the pass.
                let outpoints = entry
                    .outpoints
                    .iter()
                    .filter(|o| !pruned_outpoint_keys.contains(&format!("{}:{}", o.txid, o.vout)))
                    .cloned()
                    .collect();
                if let Err(e) = self.store.write_entry(
                    &key,
                    &StoredEntry {
                        descriptors: kept_hexes,
                        channel_id_hex: entry.channel_id_hex.clone(),
                        outpoints,
                    },
                ) {
                    log_error!(self.logger, "Failed to persist pruned entry {key}: {e}");
                }
            }
            if let Some(channel) = &entry.channel_id_hex {
                channels.insert(channel.clone());
            }
            batch_keys.push(key.clone());
            for (bytes, descriptor) in members {
                batch.push(BatchMember {
                    bytes,
                    descriptor,
                    entry_key: key.clone(),
                });
            }
        }
        if !emptied_keys.is_empty() {
            self.store.remove_entries(&emptied_keys);
        }

        if batch.is_empty() {
            self.finish_attempt(skipped > 0, None);
            return SweepPassSummary {
                swept: 0,
                skipped,
                txid: None,
            };
        }

        // ---- Stage 2: fee rate (6-block, ceil'd, clamped 2..=500 sat/vB).
        let rate_sat_per_vb = self.sweep_rate_sat_per_vb();
        let rate_sat_per_kw = (rate_sat_per_vb * 250) as u32;

        // Destination: reveal-next external, persisted (PWA
        // revealNextAddress) so a restart keeps watching it.
        let Ok(destination) = self.wallet.reveal_next_external_script() else {
            log_error!(self.logger, "Sweep aborted: no destination script");
            self.finish_attempt(true, None);
            return SweepPassSummary {
                swept: 0,
                skipped: skipped + batch.len() as u32,
                txid: None,
            };
        };

        // ---- Stage 3: all-or-nothing spend, with structural classification
        // on failure: a member failing the fee-independent signing probe is
        // structurally unsignable — remove it and retry the batch once.
        let mut spend_result = self.spend_batch(&batch, &destination, rate_sat_per_kw);
        if spend_result.is_err() {
            let poisoned: Vec<usize> = batch
                .iter()
                .enumerate()
                .filter(|(_, member)| {
                    self.descriptor_is_unsignable(&member.descriptor, &destination)
                })
                .map(|(idx, _)| idx)
                .collect();
            if !poisoned.is_empty() {
                log_error!(
                    self.logger,
                    "Sweep batch poisoned by {} structurally unsignable descriptor(s); \
                     removing and retrying",
                    poisoned.len()
                );
                self.remove_members(&mut batch, &poisoned);
                if batch.is_empty() {
                    self.finish_attempt(skipped > 0, None);
                    return SweepPassSummary {
                        swept: 0,
                        skipped,
                        txid: None,
                    };
                }
                spend_result = self.spend_batch(&batch, &destination, rate_sat_per_kw);
            }
        }
        // LDK returns a ZERO-OUTPUT tx when the change is sub-dust but the
        // fee is affordable (`maybe_add_change_output`'s middle branch) —
        // consensus-invalid, so broadcasting it can only fail. Treat it as
        // the conditional can't-pay-own-fee failure it is, so the subsidized
        // path gets a chance (the PWA instead retries the broadcast hourly).
        let spend_result = spend_result.and_then(|tx| {
            if tx.output.is_empty() {
                Err(())
            } else {
                Ok(tx)
            }
        });

        let batch_len = batch.len() as u32;
        match spend_result {
            Ok(tx) => {
                // Fee sanity (incident review): fee = tracked input values −
                // outputs, weight from the SIGNED tx.
                let input_sats: u64 = batch
                    .iter()
                    .map(|member| descriptor_value_sats(&member.descriptor))
                    .sum();
                let output_sats: u64 = tx.output.iter().map(|out| out.value.to_sat()).sum();
                let fee_sats = input_sats.saturating_sub(output_sats);
                if let Err(e) = check_fee_sanity(
                    fee_sats,
                    tx.weight().to_wu(),
                    fee_sanity_max_sat_per_kw(&self.fee_estimator),
                ) {
                    log_error!(self.logger, "Sweep refused: {e}");
                    self.finish_attempt(true, None);
                    return SweepPassSummary {
                        swept: 0,
                        skipped: skipped + batch_len,
                        txid: None,
                    };
                }

                self.broadcast.persist_pending(&tx);
                match self.broadcast.broadcast(&tx).await {
                    // LDK-only inputs: an already-known sentinel genuinely
                    // refers to OUR tx, so both outcomes are chain truth.
                    BroadcastOutcome::Accepted | BroadcastOutcome::AlreadyKnown => {
                        let txid = tx.compute_txid();
                        self.commit_swept(&batch_keys, &txid, &channels);
                        self.finish_attempt(skipped > 0, None);
                        log_info!(
                            self.logger,
                            "Swept {batch_len} output(s) in {txid} at {rate_sat_per_vb} sat/vB"
                        );
                        SweepPassSummary {
                            swept: batch_len,
                            skipped,
                            txid: Some(txid),
                        }
                    }
                    BroadcastOutcome::Failed(e) => {
                        log_error!(self.logger, "Sweep broadcast failed: {e}");
                        self.finish_attempt(true, None);
                        SweepPassSummary {
                            swept: 0,
                            skipped: skipped + batch_len,
                            txid: None,
                        }
                    }
                }
            }
            Err(()) => {
                // Conditional failure (outputs can't pay their own fee, or
                // are timelocked): try covering the shortfall with confirmed
                // on-chain funds before giving up.
                log_info!(
                    self.logger,
                    "spend_spendable_outputs failed — attempting subsidized sweep \
                     ({batch_len} descriptor(s))"
                );
                let outcome = self
                    .attempt_subsidized(&batch, &destination, rate_sat_per_vb)
                    .await;
                match outcome {
                    SubsidizedOutcome::Broadcast { txid, subsidy_sats } => {
                        self.commit_swept(&batch_keys, &txid, &channels);
                        self.finish_attempt(skipped > 0, None);
                        log_info!(
                            self.logger,
                            "Subsidized sweep rescued {batch_len} output(s) in {txid} \
                             (subsidy {subsidy_sats} sats)"
                        );
                        SweepPassSummary {
                            swept: batch_len,
                            skipped,
                            txid: Some(txid),
                        }
                    }
                    SubsidizedOutcome::Shortfall { shortfall_sats, .. } => {
                        self.finish_attempt(true, Some(shortfall_sats));
                        SweepPassSummary {
                            swept: 0,
                            skipped: skipped + batch_len,
                            txid: None,
                        }
                    }
                    SubsidizedOutcome::NotEconomical { .. } => {
                        self.finish_attempt(true, None);
                        SweepPassSummary {
                            swept: 0,
                            skipped: skipped + batch_len,
                            txid: None,
                        }
                    }
                    SubsidizedOutcome::Failed { reason } => {
                        log_error!(self.logger, "Subsidized sweep failed: {reason}");
                        self.finish_attempt(true, None);
                        SweepPassSummary {
                            swept: 0,
                            skipped: skipped + batch_len,
                            txid: None,
                        }
                    }
                }
            }
        }
    }

    /// The sweep fee rate: the cached 6-block estimate (already ceil'd and
    /// floored at 2 by the estimator's send path), clamped at 500 sat/vB
    /// (PWA `sweep.ts:367-381`).
    fn sweep_rate_sat_per_vb(&self) -> u64 {
        self.fee_estimator
            .onchain_send_rate_sat_per_vb()
            .clamp(MIN_SWEEP_RATE_SAT_PER_VB, MAX_SWEEP_RATE_SAT_PER_VB)
    }

    fn spend_batch(
        &self,
        batch: &[BatchMember],
        destination: &ScriptBuf,
        rate_sat_per_kw: u32,
    ) -> Result<Transaction, ()> {
        let refs: Vec<&SpendableOutputDescriptor> =
            batch.iter().map(|member| &member.descriptor).collect();
        self.keys_manager.spend_spendable_outputs(
            &refs,
            Vec::new(),
            destination.clone(),
            rate_sat_per_kw,
            None,
            &self.secp,
        )
    }

    /// The structural-vs-conditional probe: signing is fee-independent, so a
    /// descriptor that cannot SIGN a fee-free single-input spend of itself
    /// can never be swept — keeping it would poison the all-or-nothing batch
    /// forever (the PWA's `ldk-sign` freeze). Fee/dust problems never trip
    /// this: the probe pays the full value back out, so only signature
    /// resolution can fail. Probe-construction failures read as SIGNABLE
    /// (kept) — the fund-safe direction.
    fn descriptor_is_unsignable(
        &self,
        descriptor: &SpendableOutputDescriptor,
        probe_script: &ScriptBuf,
    ) -> bool {
        let outpoint = descriptor.spendable_outpoint();
        let tx = Transaction {
            version: bitcoin::transaction::Version::TWO,
            lock_time: bitcoin::absolute::LockTime::ZERO,
            input: vec![TxIn {
                previous_output: outpoint.into_bitcoin_outpoint(),
                script_sig: ScriptBuf::new(),
                sequence: descriptor_probe_sequence(descriptor),
                witness: Witness::new(),
            }],
            output: vec![TxOut {
                value: Amount::from_sat(descriptor_value_sats(descriptor)),
                script_pubkey: probe_script.clone(),
            }],
        };
        let Ok(mut psbt) = Psbt::from_unsigned_tx(tx) else {
            return false;
        };
        psbt.inputs[0] = descriptor.to_psbt_input(&self.secp);
        self.keys_manager
            .sign_spendable_outputs_psbt(&[descriptor], psbt, &self.secp)
            .is_err()
    }

    /// Removes structurally poisoned members from the in-memory batch AND
    /// their persisted entries (rewrite; delete when emptied), so they never
    /// re-enter a batch.
    fn remove_members(&self, batch: &mut Vec<BatchMember>, poisoned: &[usize]) {
        let poisoned_hex: HashSet<String> = poisoned
            .iter()
            .map(|idx| hex_str(&batch[*idx].bytes))
            .collect();
        let poisoned_outpoints: HashSet<String> = poisoned
            .iter()
            .map(|idx| {
                let outpoint = batch[*idx].descriptor.spendable_outpoint();
                format!("{}:{}", outpoint.txid, u32::from(outpoint.index))
            })
            .collect();
        let affected_keys: HashSet<String> = poisoned
            .iter()
            .map(|idx| batch[*idx].entry_key.clone())
            .collect();

        let mut emptied = Vec::new();
        for (key, entry) in self.store.entries() {
            if !affected_keys.contains(&key) {
                continue;
            }
            let descriptors: Vec<String> = entry
                .descriptors
                .into_iter()
                .filter(|hex| !poisoned_hex.contains(hex))
                .collect();
            if descriptors.is_empty() {
                emptied.push(key);
                continue;
            }
            let outpoints = entry
                .outpoints
                .into_iter()
                .filter(|o| !poisoned_outpoints.contains(&format!("{}:{}", o.txid, o.vout)))
                .collect();
            if let Err(e) = self.store.write_entry(
                &key,
                &StoredEntry {
                    descriptors,
                    channel_id_hex: entry.channel_id_hex,
                    outpoints,
                },
            ) {
                log_error!(
                    self.logger,
                    "Failed to persist member removal for {key}: {e}"
                );
            }
        }
        self.store.remove_entries(&emptied);

        let mut index = 0usize;
        batch.retain(|_| {
            let keep = !poisoned.contains(&index);
            index += 1;
            keep
        });
    }

    /// The fee-subsidized fallback (PWA `attemptSubsidizedSweep`,
    /// `subsidized-sweep.ts:284-450`). Construction shape: LDK's
    /// `create_spendable_outputs_psbt` at the 250 sat/kW floor is the BASE;
    /// wallet P2WPKH inputs (+ optional change) are appended as TYPED
    /// `Psbt`/`TxIn` values — native rust-bitcoin makes the PWA's byte-level
    /// PSBT surgery unnecessary. bdk's `TxBuilder::add_foreign_utxo` was
    /// considered and rejected: it cannot express per-input sequences (LDK
    /// delayed outputs need their CSV sequence, wallet inputs the RBF one)
    /// and replaces the PWA's exact largest-first/≤20/changeless selection
    /// math with bdk's coin selection.
    async fn attempt_subsidized(
        &self,
        batch: &[BatchMember],
        destination: &ScriptBuf,
        target_rate_sat_per_vb: u64,
    ) -> SubsidizedOutcome {
        let refs: Vec<&SpendableOutputDescriptor> =
            batch.iter().map(|member| &member.descriptor).collect();
        let Ok((mut psbt, ldk_weight_wu)) =
            SpendableOutputDescriptor::create_spendable_outputs_psbt(
                &self.secp,
                &refs,
                Vec::new(),
                destination.clone(),
                FLOOR_FEERATE_SAT_PER_KW,
                None,
            )
        else {
            // Duplicated descriptor, script mismatch, or value below even
            // the floor fee.
            return SubsidizedOutcome::Failed {
                reason: "ldk-create-psbt".to_string(),
            };
        };

        // LDK does not enforce dust on its outputs; a sub-dust output would
        // be rejected at relay after we spent both signatures on it.
        for output in &psbt.unsigned_tx.output {
            if output.value.to_sat() < DUST_LIMIT_SATS {
                return SubsidizedOutcome::Failed {
                    reason: "sub-dust-output".to_string(),
                };
            }
        }

        let ldk_input_sats: u64 = psbt
            .inputs
            .iter()
            .filter_map(|input| input.witness_utxo.as_ref())
            .map(|utxo| utxo.value.to_sat())
            .sum();
        let ldk_output_sats: u64 = psbt
            .unsigned_tx
            .output
            .iter()
            .map(|out| out.value.to_sat())
            .sum();
        let Some(ldk_fee_sats) = ldk_input_sats.checked_sub(ldk_output_sats) else {
            return SubsidizedOutcome::Failed {
                reason: "negative-ldk-fee".to_string(),
            };
        };

        // Net-positive policy: never spend more on-chain than the sweep
        // rescues. The rescued value is what the destination output DELIVERS
        // (ldk_output_sats), not the gross input value — gating on inputs
        // would allow a rescue that is net-negative by up to the floor fee.
        let minimum_subsidy = fee_for_weight(
            ldk_weight_wu + SUBSIDY_INPUT_WEIGHT_WU + CHANGE_OUTPUT_WEIGHT_WU,
            target_rate_sat_per_vb,
        )
        .saturating_sub(ldk_fee_sats);
        if minimum_subsidy == 0 {
            return SubsidizedOutcome::Failed {
                reason: "no-subsidy-needed".to_string(),
            };
        }
        if minimum_subsidy >= ldk_output_sats {
            return SubsidizedOutcome::NotEconomical {
                needed_subsidy_sats: minimum_subsidy,
                pending_sats: ldk_input_sats,
            };
        }

        let candidates = self.confirmed_p2wpkh_candidates();
        let selection = select_subsidy_inputs(
            &candidates,
            ldk_weight_wu,
            ldk_fee_sats,
            target_rate_sat_per_vb,
            (self.reserve_sats)(),
        );
        let selection = match selection {
            SelectionOutcome::Selected(selection) => selection,
            SelectionOutcome::Shortfall {
                needed_subsidy_sats,
                available_sats,
            } => {
                return SubsidizedOutcome::Shortfall {
                    needed_subsidy_sats,
                    available_sats,
                    shortfall_sats: needed_subsidy_sats.saturating_sub(available_sats).max(1),
                }
            }
        };
        if selection.subsidy_sats >= ldk_output_sats {
            return SubsidizedOutcome::NotEconomical {
                needed_subsidy_sats: selection.subsidy_sats,
                pending_sats: ldk_input_sats,
            };
        }

        // Change output (with-change variant): a wallet-internal P2WPKH; the
        // weight math assumes 22-byte scripts.
        let change = match selection.change_sats {
            Some(change_sats) => {
                use lightning::sign::ChangeDestinationSourceSync as _;
                let Ok(script) = self.wallet.get_change_destination_script() else {
                    return SubsidizedOutcome::Failed {
                        reason: "change-script".to_string(),
                    };
                };
                if script.len() != 22 {
                    return SubsidizedOutcome::Failed {
                        reason: "unexpected-change-script".to_string(),
                    };
                }
                Some(TxOut {
                    value: Amount::from_sat(change_sats),
                    script_pubkey: script,
                })
            }
            None => None,
        };

        // Append the wallet inputs (+ change) as typed PSBT members. LDK's
        // PSBT already carries per-descriptor sequences; wallet inputs get
        // the RBF sequence (PWA parity).
        for input in &selection.selected {
            psbt.unsigned_tx.input.push(TxIn {
                previous_output: input.outpoint,
                script_sig: ScriptBuf::new(),
                sequence: RBF_SEQUENCE,
                witness: Witness::new(),
            });
            psbt.inputs.push(bitcoin::psbt::Input {
                witness_utxo: Some(TxOut {
                    value: Amount::from_sat(input.value_sats),
                    script_pubkey: input.script_pubkey.clone(),
                }),
                ..Default::default()
            });
        }
        if let Some(change) = change {
            psbt.unsigned_tx.output.push(change);
            psbt.outputs.push(bitcoin::psbt::Output::default());
        }

        // Independent fee re-verification BEFORE anything signs: the PSBT's
        // own arithmetic must match the selection to the sat.
        match psbt.fee() {
            Ok(fee) if fee.to_sat() == selection.total_fee_sats => {}
            Ok(fee) => {
                log_error!(
                    self.logger,
                    "Subsidized sweep fee mismatch: computed {}, PSBT says {}",
                    selection.total_fee_sats,
                    fee.to_sat()
                );
                return SubsidizedOutcome::Failed {
                    reason: "fee-mismatch".to_string(),
                };
            }
            Err(e) => {
                return SubsidizedOutcome::Failed {
                    reason: format!("fee-unreadable: {e}"),
                };
            }
        }

        // Fee-sanity middleware (incident review): estimated signed weight.
        let estimated_weight_wu = ldk_weight_wu
            + selection.selected.len() as u64 * SUBSIDY_INPUT_WEIGHT_WU
            + if selection.change_sats.is_some() {
                CHANGE_OUTPUT_WEIGHT_WU
            } else {
                0
            };
        if let Err(e) = check_fee_sanity(
            selection.total_fee_sats,
            estimated_weight_wu,
            fee_sanity_max_sat_per_kw(&self.fee_estimator),
        ) {
            log_error!(self.logger, "Subsidized sweep refused: {e}");
            return SubsidizedOutcome::Failed {
                reason: e.to_string(),
            };
        }

        // Dual sign: LDK first (its inputs finalize), then bdk for the
        // wallet inputs (trust_witness_utxo — the LDK-produced PSBT carries
        // only witness_utxo; same rationale as the anchor-CPFP path).
        let Ok(mut psbt) = self
            .keys_manager
            .sign_spendable_outputs_psbt(&refs, psbt, &self.secp)
        else {
            return SubsidizedOutcome::Failed {
                reason: "ldk-sign".to_string(),
            };
        };
        let finalized = match self.wallet.sign_psbt_trusted(&mut psbt) {
            Ok(finalized) => finalized,
            Err(e) => {
                return SubsidizedOutcome::Failed {
                    reason: format!("bdk-sign: {e}"),
                }
            }
        };
        // extract_tx does NOT reject unfinalized inputs, so the sign()
        // return value is the only missing-signature gate.
        if !finalized {
            return SubsidizedOutcome::Failed {
                reason: "bdk-sign-incomplete".to_string(),
            };
        }
        match psbt.fee() {
            Ok(fee) if fee.to_sat() == selection.total_fee_sats => {}
            _ => {
                return SubsidizedOutcome::Failed {
                    reason: "post-sign-fee-mismatch".to_string(),
                }
            }
        }
        let Ok(tx) = psbt.extract_tx() else {
            return SubsidizedOutcome::Failed {
                reason: "extract-tx".to_string(),
            };
        };
        let txid = tx.compute_txid();

        self.broadcast.persist_pending(&tx);
        match self.broadcast.broadcast(&tx).await {
            BroadcastOutcome::Accepted => {}
            BroadcastOutcome::AlreadyKnown => {
                // The broadcaster maps "inputs missing or spent" (and
                // similar) to a success sentinel. For the plain sweep that's
                // safe; HERE a concurrently spent wallet input produces the
                // same error, and trusting it would delete the descriptors
                // while the funds never moved. Only believe it if the chain
                // actually knows the tx.
                if !self.broadcast.tx_known(&txid).await {
                    return SubsidizedOutcome::Failed {
                        reason: "broadcast-ambiguous".to_string(),
                    };
                }
            }
            BroadcastOutcome::Failed(e) => {
                return SubsidizedOutcome::Failed {
                    reason: format!("broadcast: {e}"),
                };
            }
        }

        self.mark_subsidy_inputs_spent(&tx, &selection.selected);
        SubsidizedOutcome::Broadcast {
            txid,
            subsidy_sats: selection.subsidy_sats,
        }
    }

    /// Confirmed wallet P2WPKH UTXOs, minus this session's already-consumed
    /// subsidy outpoints, sorted largest-first
    /// (`listConfirmedP2wpkhUtxos`, `subsidized-sweep.ts:178-201`).
    fn confirmed_p2wpkh_candidates(&self) -> Vec<SubsidyInput> {
        let reserved = self.spent_subsidy_outpoints.lock().unwrap();
        let mut candidates: Vec<SubsidyInput> = self
            .wallet
            .confirmed_utxos()
            .into_iter()
            .filter(|(outpoint, txout)| {
                !reserved.contains(outpoint) && txout.script_pubkey.is_p2wpkh()
            })
            .map(|(outpoint, txout)| SubsidyInput {
                outpoint,
                value_sats: txout.value.to_sat(),
                script_pubkey: txout.script_pubkey,
            })
            .collect();
        candidates.sort_by(|a, b| b.value_sats.cmp(&a.value_sats));
        candidates
    }

    /// Make the spend visible immediately (PWA `markSubsidyInputsSpent`,
    /// `subsidized-sweep.ts:248-277`): reserve the outpoints against
    /// re-selection by a later sweep, and register the tx with the wallet
    /// graph so user sends exclude them before the next chain sync.
    /// Registration failure is non-fatal — the reservation set still guards
    /// the sweep path.
    fn mark_subsidy_inputs_spent(&self, tx: &Transaction, selected: &[SubsidyInput]) {
        {
            let mut reserved = self.spent_subsidy_outpoints.lock().unwrap();
            for input in selected {
                reserved.insert(input.outpoint);
            }
        }
        if let Err(e) = self
            .wallet
            .apply_unconfirmed_tx(tx.clone(), crate::util::unix_now().as_secs())
        {
            log_error!(
                self.logger,
                "Failed to register the subsidized sweep with the wallet \
                 (relying on the next sync): {e}"
            );
        }
    }

    /// Success epilogue: delete the consumed entries and attribute the sweep
    /// txid to every contributing channel's close record (U10/KTD-7 —
    /// attribution by the channelId persisted with each descriptor, never by
    /// "the sweep my event triggered").
    fn commit_swept(&self, batch_keys: &[String], txid: &Txid, channels: &HashSet<String>) {
        self.store.remove_entries(batch_keys);
        if !channels.is_empty() {
            record_sweep_tx(
                &self.close_records,
                &txid.to_string(),
                channels,
                crate::util::unix_now().as_millis() as u64,
            );
        }
    }

    /// Records the attempt outcome and fires `SweepStateChanged` when the
    /// pending state changed (PWA `notifySweepStateChanged`). Every finished
    /// attempt that touched entries notifies — the UI re-reads on this.
    fn finish_attempt(&self, last_attempt_failed: bool, shortfall_sats: Option<u64>) {
        self.store.set_flags(last_attempt_failed, shortfall_sats);
        self.event_sink.emit(CoreEvent::SweepStateChanged);
    }
}

/// The auto-recover seam (U10): a sweep attempt that lands outputs reports
/// swept > 0, transitioning the recovery banner to `sweep_confirmed`.
impl RecoverySweeper for SweepEngine {
    fn attempt_sweep(&self) -> BoxFuture<'_, u64> {
        Box::pin(async move { u64::from(self.sweep_once().await.swept) })
    }
}

// ---------------------------------------------------------------------------
// Descriptor helpers
// ---------------------------------------------------------------------------

/// The output value a descriptor spends, in sats.
pub(crate) fn descriptor_value_sats(descriptor: &SpendableOutputDescriptor) -> u64 {
    match descriptor {
        SpendableOutputDescriptor::StaticOutput { output, .. } => output.value.to_sat(),
        SpendableOutputDescriptor::DelayedPaymentOutput(descriptor) => {
            descriptor.output.value.to_sat()
        }
        SpendableOutputDescriptor::StaticPaymentOutput(descriptor) => {
            descriptor.output.value.to_sat()
        }
    }
}

/// The sequence a real spend of this descriptor must carry (LDK's signers
/// validate it): CSV `to_self_delay` for delayed outputs, CSV-1 for anchor
/// static-payment outputs, zero otherwise.
fn descriptor_probe_sequence(descriptor: &SpendableOutputDescriptor) -> Sequence {
    match descriptor {
        SpendableOutputDescriptor::StaticOutput { .. } => Sequence::ZERO,
        SpendableOutputDescriptor::DelayedPaymentOutput(descriptor) => {
            Sequence(u32::from(descriptor.to_self_delay))
        }
        SpendableOutputDescriptor::StaticPaymentOutput(descriptor) => {
            if descriptor.needs_csv_1_for_spend() {
                Sequence::from_consensus(1)
            } else {
                Sequence::ZERO
            }
        }
    }
}

/// A fresh UUID-format entry key from the node's entropy source (the PWA's
/// `crypto.randomUUID()`).
fn new_entry_key(keys_manager: &KeysManager) -> String {
    use lightning::sign::EntropySource as _;
    let bytes = keys_manager.get_secure_random_bytes();
    format!(
        "{}-{}-{}-{}-{}",
        hex_str(&bytes[0..4]),
        hex_str(&bytes[4..6]),
        hex_str(&bytes[6..8]),
        hex_str(&bytes[8..10]),
        hex_str(&bytes[10..16]),
    )
}

fn decode_hex(hex: &str) -> Option<Vec<u8>> {
    if !hex.len().is_multiple_of(2) {
        return None;
    }
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;

    use bitcoin::hashes::Hash as _;
    use lightning::chain::transaction::OutPoint as LdkOutPoint;
    use lightning::sign::SignerProvider as _;

    use super::*;
    use crate::keys::{derive_wallet_keys, parse_mnemonic, tests::TEST_MNEMONIC};

    #[derive(Default)]
    struct CapturingSink(Mutex<Vec<CoreEvent>>);

    impl EventSink for CapturingSink {
        fn emit(&self, event: CoreEvent) {
            self.0.lock().unwrap().push(event);
        }
    }

    /// Scriptable [`SweepBroadcast`]: outcome per call, chain-knowledge
    /// flag, and a record of every broadcast tx.
    struct MockBroadcast {
        outcome: Mutex<BroadcastOutcome>,
        known: AtomicBool,
        broadcasts: Mutex<Vec<Transaction>>,
    }

    impl MockBroadcast {
        fn new(outcome: BroadcastOutcome) -> Self {
            Self {
                outcome: Mutex::new(outcome),
                known: AtomicBool::new(false),
                broadcasts: Mutex::new(Vec::new()),
            }
        }

        fn broadcast_count(&self) -> usize {
            self.broadcasts.lock().unwrap().len()
        }

        fn last_tx(&self) -> Option<Transaction> {
            self.broadcasts.lock().unwrap().last().cloned()
        }
    }

    impl SweepBroadcast for MockBroadcast {
        fn persist_pending(&self, _tx: &Transaction) {}

        fn broadcast<'a>(&'a self, tx: &'a Transaction) -> BoxFuture<'a, BroadcastOutcome> {
            self.broadcasts.lock().unwrap().push(tx.clone());
            let outcome = self.outcome.lock().unwrap().clone();
            Box::pin(async move { outcome })
        }

        fn tx_known<'a>(&'a self, _txid: &'a Txid) -> BoxFuture<'a, bool> {
            let known = self.known.load(Ordering::Acquire);
            Box::pin(async move { known })
        }
    }

    struct Harness {
        _dir: tempfile::TempDir,
        engine: SweepEngine,
        store: Arc<SweepStore>,
        wallet: Arc<OnchainWallet>,
        keys_manager: Arc<KeysManager>,
        close_records: Arc<CloseRecordStore>,
        sink: Arc<CapturingSink>,
        broadcast: Arc<MockBroadcast>,
        fee_estimator: Arc<CachedFeeEstimator>,
    }

    fn harness(reserve_sats: u64, outcome: BroadcastOutcome) -> Harness {
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
        let store = Arc::new(SweepStore::new(Arc::clone(&kv_store), Arc::clone(&logger)));
        let close_records = Arc::new(CloseRecordStore::new(kv_store, Arc::clone(&logger)));
        let sink = Arc::new(CapturingSink::default());
        let broadcast = Arc::new(MockBroadcast::new(outcome));
        let fee_estimator = Arc::new(CachedFeeEstimator::new());
        let engine = SweepEngine::new(
            Arc::clone(&store),
            Arc::clone(&keys_manager),
            Arc::clone(&wallet),
            Arc::clone(&broadcast) as Arc<dyn SweepBroadcast>,
            Arc::clone(&fee_estimator),
            Arc::clone(&close_records),
            Arc::new(move || reserve_sats),
            Arc::clone(&sink) as Arc<dyn EventSink>,
            logger,
        );
        Harness {
            _dir: dir,
            engine,
            store,
            wallet,
            keys_manager,
            close_records,
            sink,
            broadcast,
            fee_estimator,
        }
    }

    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(future)
    }

    /// A `StaticOutput` at `spk`; `salt` gives each a distinct outpoint.
    fn static_output(
        spk: ScriptBuf,
        value_sats: u64,
        salt: u8,
        channel_keys_id: Option<[u8; 32]>,
    ) -> SpendableOutputDescriptor {
        SpendableOutputDescriptor::StaticOutput {
            outpoint: LdkOutPoint {
                txid: Txid::from_byte_array([salt; 32]),
                index: 0,
            },
            output: TxOut {
                value: Amount::from_sat(value_sats),
                script_pubkey: spk,
            },
            channel_keys_id,
        }
    }

    /// The one StaticOutput script the `KeysManager` CAN sign: its own
    /// destination script.
    fn signable_spk(keys_manager: &KeysManager) -> ScriptBuf {
        keys_manager.get_destination_script([0u8; 32]).unwrap()
    }

    fn foreign_spk() -> ScriptBuf {
        ScriptBuf::new_p2wpkh(&bitcoin::WPubkeyHash::from_byte_array([0x42; 20]))
    }

    fn sweep_events(sink: &CapturingSink) -> usize {
        sink.0
            .lock()
            .unwrap()
            .iter()
            .filter(|e| matches!(e, CoreEvent::SweepStateChanged))
            .count()
    }

    /// Five 200-sat signable StaticOutputs: at a 2 sat/vB target the plain
    /// spend yields LDK's zero-output degenerate tx (conditional failure)
    /// while the subsidized rescue is JUST net-positive — the worked window
    /// (see the module docs' PWA math).
    fn near_dust_batch(keys_manager: &KeysManager) -> Vec<SpendableOutputDescriptor> {
        (0..5)
            .map(|i| static_output(signable_spk(keys_manager), 200, 0x10 + i, None))
            .collect()
    }

    fn set_sweep_rate(harness: &Harness, sat_per_vb: f64) {
        harness
            .fee_estimator
            .set_onchain_send_rate(Some(sat_per_vb));
    }

    // ---------- selection math (PWA subsidized-sweep.ts:104-164) ----------

    fn candidate(value_sats: u64, salt: u8) -> SubsidyInput {
        SubsidyInput {
            outpoint: OutPoint {
                txid: Txid::from_byte_array([salt; 32]),
                vout: 0,
            },
            value_sats,
            script_pubkey: foreign_spk(),
        }
    }

    /// The PWA's worked example (subsidized-sweep.test.ts:271-279): LDK tx
    /// 439 wu paying 110 sats at the floor, target 10 sat/vB. One input +
    /// change → 835 wu → 209 vB → 2,090 fee, 1,980 subsidy.
    #[test]
    fn selection_selects_one_utxo_with_change_worked_example() {
        let result = select_subsidy_inputs(&[candidate(50_000, 1)], 439, 110, 10, 0);
        assert_eq!(
            result,
            SelectionOutcome::Selected(SubsidySelection {
                selected: vec![candidate(50_000, 1)],
                change_sats: Some(48_020),
                total_fee_sats: 2_090,
                subsidy_sats: 1_980,
            })
        );
    }

    /// Changeless variant when the change would be dust, with bounded
    /// overpay (PWA test:281-292): changeless 711 wu → 178 vB → 1,780 fee →
    /// 1,670 needed; the whole 2,100-sat input is contributed.
    #[test]
    fn selection_falls_back_to_changeless_when_change_would_be_dust() {
        let result = select_subsidy_inputs(&[candidate(2_100, 1)], 439, 110, 10, 0);
        assert_eq!(
            result,
            SelectionOutcome::Selected(SubsidySelection {
                selected: vec![candidate(2_100, 1)],
                change_sats: None,
                total_fee_sats: 2_210,
                subsidy_sats: 2_100,
            })
        );
    }

    /// PWA test:294-304: n=2 changeless — 983 wu → 246 vB → 2,460 fee →
    /// 2,350 needed ≤ 2,700 selected.
    #[test]
    fn selection_adds_a_second_utxo_when_the_first_cannot_cover() {
        let result =
            select_subsidy_inputs(&[candidate(1_500, 1), candidate(1_200, 2)], 439, 110, 10, 0);
        assert_eq!(
            result,
            SelectionOutcome::Selected(SubsidySelection {
                selected: vec![candidate(1_500, 1), candidate(1_200, 2)],
                change_sats: None,
                total_fee_sats: 2_810,
                subsidy_sats: 2_700,
            })
        );
    }

    /// PWA test:306-316: neededWithChange(1) = 1,980 > 1,900 spendable, but
    /// changeless needs only 1,670 — must not report a shortfall.
    #[test]
    fn selection_rescues_via_changeless_when_with_change_exceeds_spendable() {
        let result = select_subsidy_inputs(&[candidate(1_900, 1)], 439, 110, 10, 0);
        assert_eq!(
            result,
            SelectionOutcome::Selected(SubsidySelection {
                selected: vec![candidate(1_900, 1)],
                change_sats: None,
                total_fee_sats: 2_010,
                subsidy_sats: 1_900,
            })
        );
    }

    /// U11 scenario: the RESERVE is untouched — the spendable budget is
    /// total minus reserve, and a selection that needs more reports a
    /// shortfall instead of dipping into the anchor reserve (PWA
    /// test:328-331).
    #[test]
    fn selection_honors_the_anchor_reserve() {
        let result = select_subsidy_inputs(&[candidate(50_000, 1)], 439, 110, 10, 48_500);
        assert_eq!(
            result,
            SelectionOutcome::Shortfall {
                needed_subsidy_sats: 1_980,
                available_sats: 1_500,
            }
        );
    }

    #[test]
    fn selection_reports_shortfall_when_candidates_cannot_cover() {
        assert_eq!(
            select_subsidy_inputs(&[candidate(500, 1)], 439, 110, 10, 0),
            SelectionOutcome::Shortfall {
                needed_subsidy_sats: 1_980,
                available_sats: 500,
            }
        );
        assert_eq!(
            select_subsidy_inputs(&[], 439, 110, 10, 0),
            SelectionOutcome::Shortfall {
                needed_subsidy_sats: 1_980,
                available_sats: 0,
            }
        );
    }

    /// U11 scenario: never more than 20 subsidy inputs (PWA test:333-341):
    /// neededWithChange(20) = 439 + 20x272 + 124 = 6,003 wu → 1,501 vB →
    /// 15,010 minus the 110 ldk fee. A broken or removed cap changes this.
    #[test]
    fn selection_caps_at_twenty_inputs() {
        let candidates: Vec<SubsidyInput> =
            (0..25).map(|i| candidate(10, 0x30 + i as u8)).collect();
        assert_eq!(
            select_subsidy_inputs(&candidates, 439, 110, 10, 0),
            SelectionOutcome::Shortfall {
                needed_subsidy_sats: 14_900,
                available_sats: 250,
            }
        );
    }

    #[test]
    fn fee_for_weight_rounds_weight_up_to_vbytes() {
        assert_eq!(fee_for_weight(1_000, 10), 2_500);
        assert_eq!(fee_for_weight(1_001, 10), 2_510);
        assert_eq!(fee_for_weight(4, 3), 3);
    }

    // ---------- store: dedup, pending info, channel blocking ----------

    /// U11 scenario (event replay): tracking the same outputs twice persists
    /// ONE entry, and the pending banner counts each output once.
    #[test]
    fn replayed_events_are_deduped_by_descriptor_and_outpoint() {
        let h = harness(0, BroadcastOutcome::Accepted);
        let outputs = vec![static_output(foreign_spk(), 40_000, 1, None)];

        assert_eq!(
            h.engine
                .track_spendable_outputs(&outputs, Some("chan1".into()))
                .unwrap(),
            1
        );
        // The replay: same descriptor, same outpoint — nothing new persists.
        assert_eq!(
            h.engine
                .track_spendable_outputs(&outputs, Some("chan1".into()))
                .unwrap(),
            0
        );

        let info = h.store.pending_info().expect("pending");
        assert_eq!(info.entry_count, 1);
        assert_eq!(info.descriptor_count, 1);
        assert_eq!(info.pending_sats, 40_000);
        assert!(!info.has_unknown_value);
    }

    /// U11 scenario (lower-bound semantics): unreadable value data flags
    /// `has_unknown_value` and never gates the banner.
    #[test]
    fn pending_info_is_a_lower_bound_with_unknown_value_flag() {
        let h = harness(0, BroadcastOutcome::Accepted);
        h.store
            .write_entry(
                "entry-a",
                &StoredEntry {
                    descriptors: vec!["aa".into()],
                    channel_id_hex: Some("chan1".into()),
                    outpoints: vec![StoredOutpoint {
                        txid: "t1".into(),
                        vout: 0,
                        value_sats: "not-a-number".into(),
                    }],
                },
            )
            .unwrap();
        h.store
            .write_entry(
                "entry-b",
                &StoredEntry {
                    descriptors: vec!["bb".into()],
                    channel_id_hex: None,
                    outpoints: vec![StoredOutpoint {
                        txid: "t2".into(),
                        vout: 1,
                        value_sats: "12345".into(),
                    }],
                },
            )
            .unwrap();

        let info = h.store.pending_info().unwrap();
        assert_eq!(info.entry_count, 2);
        assert_eq!(info.pending_sats, 12_345, "known values only — lower bound");
        assert!(info.has_unknown_value);

        // The U10 completion blocker: channels with un-swept outputs.
        assert_eq!(
            h.store.pending_channel_ids(),
            ["chan1".to_string()].into_iter().collect::<HashSet<_>>()
        );
    }

    #[test]
    fn pending_info_is_none_when_nothing_is_tracked() {
        let h = harness(0, BroadcastOutcome::Accepted);
        assert!(h.store.pending_info().is_none());
    }

    // ---------- StaticOutput exclusion (sweep.ts:117-149) ----------

    /// U11 scenario: a StaticOutput paying a script the wallet already owns
    /// is excluded BEFORE persist; a foreign StaticOutput is kept (the
    /// fund-safe direction).
    #[test]
    fn wallet_owned_static_outputs_are_excluded_before_persist() {
        let h = harness(0, BroadcastOutcome::Accepted);
        let mine = h.wallet.next_unused_address_script().unwrap();
        let outputs = vec![
            static_output(mine, 30_000, 1, None),
            static_output(foreign_spk(), 20_000, 2, None),
        ];
        assert_eq!(h.engine.track_spendable_outputs(&outputs, None).unwrap(), 1);
        let info = h.store.pending_info().unwrap();
        assert_eq!(info.descriptor_count, 1);
        assert_eq!(info.pending_sats, 20_000, "only the foreign output waits");
    }

    /// U11 scenario (post-recovery re-derivation): after a cross-device
    /// recovery the destination index is not revealed yet, so `is_mine` is
    /// false for a script the wallet CAN spend — the `channel_keys_id`
    /// re-derivation (U1's deterministic index scheme) must still exclude
    /// it, and the confirmed match reveals the index so the funds show.
    #[test]
    fn post_recovery_static_output_is_excluded_by_channel_keys_id_rederivation() {
        let h = harness(0, BroadcastOutcome::Accepted);
        // First 4 bytes big-endian = 735 -> destination index 735, far
        // beyond the fresh wallet's revealed indexes and lookahead.
        let mut keys_id = [0u8; 32];
        keys_id[2] = 0x02;
        keys_id[3] = 0xDF;
        assert_eq!(crate::signer::destination_index(&keys_id), 735);
        let spk = h.wallet.peek_external_script(735);
        assert!(
            !h.wallet.is_mine_script(&spk),
            "precondition: the unrevealed index is invisible to is_mine"
        );

        let outputs = vec![static_output(spk.clone(), 15_000, 3, Some(keys_id))];
        assert_eq!(
            h.engine.track_spendable_outputs(&outputs, None).unwrap(),
            0,
            "the re-derived match is excluded"
        );
        assert!(h.store.pending_info().is_none());
        assert!(
            h.wallet.is_mine_script(&spk),
            "the confirmed match reveals the index so bdk tracks the funds"
        );

        // An all-zero keys id never re-derives (LDK's None-equivalent).
        let outputs = vec![static_output(
            h.wallet.peek_external_script(9_999),
            15_000,
            4,
            Some([0u8; 32]),
        )];
        assert_eq!(
            h.engine.track_spendable_outputs(&outputs, None).unwrap(),
            1,
            "a zeroed keys id must not derive index 0 and misclassify"
        );
    }

    // ---------- the plain sweep pass ----------

    /// Happy path: one signable output sweeps in one tx at the 6-block rate;
    /// the entry is deleted AFTER the accepted broadcast, the sweep txid is
    /// attributed to the source channel's close record (KTD-7), and
    /// `SweepStateChanged` fires.
    #[test]
    fn plain_sweep_deletes_entries_and_attributes_the_txid() {
        let h = harness(0, BroadcastOutcome::Accepted);
        let outputs = vec![static_output(
            signable_spk(&h.keys_manager),
            100_000,
            1,
            None,
        )];
        h.engine
            .track_spendable_outputs(&outputs, Some("chan1".into()))
            .unwrap();

        let summary = block_on(h.engine.sweep_once());
        assert_eq!(summary.swept, 1);
        let txid = summary.txid.expect("swept txid");
        assert_eq!(h.broadcast.broadcast_count(), 1);
        let tx = h.broadcast.last_tx().unwrap();
        assert!(!tx.input[0].witness.is_empty(), "the sweep tx is signed");

        assert!(h.store.pending_info().is_none(), "entries deleted");
        let record = h.close_records.get("chan1").expect("close record");
        assert!(
            record.txs.iter().any(|t| t.txid == txid.to_string()
                && t.role == crate::close_records::CloseTxRole::Sweep),
            "sweep txid attributed to the contributing channel"
        );
        assert!(sweep_events(&h.sink) >= 1);

        // The recovery seam sees the swept count.
        let h2 = harness(0, BroadcastOutcome::Accepted);
        h2.engine
            .track_spendable_outputs(
                &[static_output(
                    signable_spk(&h2.keys_manager),
                    90_000,
                    2,
                    None,
                )],
                None,
            )
            .unwrap();
        assert_eq!(block_on(RecoverySweeper::attempt_sweep(&h2.engine)), 1);
    }

    /// U11 guard (descriptor-deletion discipline): a FAILED broadcast keeps
    /// every entry — descriptors are deleted only after chain truth.
    #[test]
    fn failed_broadcast_keeps_descriptors() {
        let h = harness(0, BroadcastOutcome::Failed("boom".into()));
        h.engine
            .track_spendable_outputs(
                &[static_output(
                    signable_spk(&h.keys_manager),
                    100_000,
                    1,
                    None,
                )],
                Some("chan1".into()),
            )
            .unwrap();

        let summary = block_on(h.engine.sweep_once());
        assert_eq!(summary.swept, 0);
        let info = h.store.pending_info().expect("entries kept");
        assert!(info.last_attempt_failed);
        assert_eq!(info.pending_sats, 100_000);
    }

    /// U11 scenario (structural vs conditional): a batch poisoned by a
    /// structurally unsignable member (a foreign StaticOutput that slipped
    /// past exclusion) fails all-or-nothing; the poisoned member is REMOVED
    /// from the store and the batch retried — the remaining output sweeps.
    #[test]
    fn poisoned_member_is_removed_and_the_batch_retried() {
        let h = harness(0, BroadcastOutcome::Accepted);
        // Bypass track()'s exclusion: persist the poisoned entry directly,
        // as an entry written by an earlier buggy session would be.
        let signable = static_output(signable_spk(&h.keys_manager), 100_000, 1, None);
        let poisoned = static_output(foreign_spk(), 50_000, 2, None);
        h.store
            .write_entry(
                "mixed",
                &StoredEntry {
                    descriptors: vec![hex_str(&signable.encode()), hex_str(&poisoned.encode())],
                    channel_id_hex: Some("chan1".into()),
                    outpoints: vec![
                        StoredOutpoint {
                            txid: LdkOutPoint {
                                txid: Txid::from_byte_array([1; 32]),
                                index: 0,
                            }
                            .txid
                            .to_string(),
                            vout: 0,
                            value_sats: "100000".into(),
                        },
                        StoredOutpoint {
                            txid: Txid::from_byte_array([2; 32]).to_string(),
                            vout: 0,
                            value_sats: "50000".into(),
                        },
                    ],
                },
            )
            .unwrap();

        let summary = block_on(h.engine.sweep_once());
        assert_eq!(summary.swept, 1, "the healthy member swept after pruning");
        assert!(
            h.store.pending_info().is_none(),
            "the poisoned member is gone from the store, not retried forever"
        );
        // The broadcast tx spends ONLY the signable outpoint.
        let tx = h.broadcast.last_tx().unwrap();
        assert_eq!(tx.input.len(), 1);
        assert_eq!(
            tx.input[0].previous_output.txid,
            Txid::from_byte_array([1; 32])
        );
    }

    /// The probe itself: signing is fee-independent — a near-dust but OURS
    /// descriptor is NOT structural; a foreign one is.
    #[test]
    fn signability_probe_separates_structural_from_conditional() {
        let h = harness(0, BroadcastOutcome::Accepted);
        let destination = h.wallet.peek_external_script(0);
        let ours_near_dust = static_output(signable_spk(&h.keys_manager), 200, 1, None);
        let foreign = static_output(foreign_spk(), 1_000_000, 2, None);
        assert!(!h
            .engine
            .descriptor_is_unsignable(&ours_near_dust, &destination));
        assert!(h.engine.descriptor_is_unsignable(&foreign, &destination));
    }

    // ---------- the subsidized fallback ----------

    /// U11 end-to-end: five 200-sat outputs cannot pay their own fee at a
    /// 2 sat/vB target (LDK's spend degenerates to a zero-output tx), so the
    /// SUBSIDIZED path rescues them: LDK PSBT at the 250 sat/kW floor +
    /// one confirmed wallet P2WPKH input, dual-signed, fee-verified to the
    /// sat, RBF-signaled, broadcast, entries deleted, inputs reserved and
    /// applied unconfirmed.
    #[test]
    fn subsidized_sweep_rescues_near_dust_outputs_end_to_end() {
        let h = harness(0, BroadcastOutcome::Accepted);
        crate::wallet::test_support::fund_confirmed(&h.wallet, 50_000);
        set_sweep_rate(&h, 2.0);
        let (wallet_outpoint, _) = h.wallet.confirmed_utxos()[0];

        h.engine
            .track_spendable_outputs(&near_dust_batch(&h.keys_manager), Some("chan1".into()))
            .unwrap();
        let summary = block_on(h.engine.sweep_once());
        assert_eq!(summary.swept, 5, "all five rescued in one tx");
        let txid = summary.txid.unwrap();
        assert!(h.store.pending_info().is_none(), "entries deleted");

        let tx = h.broadcast.last_tx().unwrap();
        assert_eq!(tx.compute_txid(), txid);
        assert_eq!(tx.input.len(), 6, "5 LDK inputs + 1 wallet subsidy input");
        // Pinned to LDK's weight math: expected max weight 1,523 wu for the
        // 5-input floor PSBT + 396 wu subsidy overhead → 480 vB x 2 = 960
        // sats total fee (an LDK weight-estimate change shifts this pin).
        let input_sats = 5 * 200 + 50_000u64;
        let output_sats: u64 = tx.output.iter().map(|o| o.value.to_sat()).sum();
        assert_eq!(input_sats - output_sats, 960, "fee verified to the sat");
        assert_eq!(tx.output.len(), 2, "destination + subsidy change");
        // Every input signed (dual-sign: LDK's 5 + bdk's 1).
        assert!(tx.input.iter().all(|input| !input.witness.is_empty()));
        // The wallet input signals RBF.
        let wallet_input = tx
            .input
            .iter()
            .find(|input| input.previous_output == wallet_outpoint)
            .expect("the subsidy input rides the tx");
        assert_eq!(wallet_input.sequence, RBF_SEQUENCE);

        // Sweep attribution reached the close record.
        let record = h.close_records.get("chan1").unwrap();
        assert!(record.txs.iter().any(|t| t.txid == txid.to_string()));

        // Session reservation + wallet visibility: the consumed outpoint is
        // reserved and the unconfirmed spend registered, so a second pass
        // cannot re-select it (RBF-replacing the first tx after its
        // descriptors were deleted was the fund-loss mode).
        assert!(h
            .engine
            .spent_subsidy_outpoints
            .lock()
            .unwrap()
            .contains(&wallet_outpoint));
        assert!(h.engine.confirmed_p2wpkh_candidates().is_empty());
        assert!(
            !h.wallet.owns_unspent_outpoint(&wallet_outpoint),
            "apply_unconfirmed_txs made the spend visible to coin selection"
        );
    }

    /// U11 scenario (net-positive gate): at a high target rate the subsidy
    /// would exceed the rescued value — NotEconomical, nothing broadcast,
    /// descriptors kept.
    #[test]
    fn subsidized_sweep_refuses_a_net_negative_rescue() {
        let h = harness(0, BroadcastOutcome::Accepted);
        crate::wallet::test_support::fund_confirmed(&h.wallet, 50_000);
        set_sweep_rate(&h, 5.0);

        h.engine
            .track_spendable_outputs(&near_dust_batch(&h.keys_manager), None)
            .unwrap();
        let summary = block_on(h.engine.sweep_once());
        assert_eq!(summary.swept, 0);
        assert_eq!(h.broadcast.broadcast_count(), 0, "nothing broadcast");
        let info = h.store.pending_info().expect("descriptors kept");
        assert!(info.last_attempt_failed);
        assert!(!info.needs_onchain_funds, "not a shortfall — a policy no");
    }

    /// U11 scenario (shortfall + add-funds UX): the rescue is economical but
    /// the confirmed balance cannot cover the subsidy — shortfall state with
    /// the estimated missing sats, driving `needs_onchain_funds`.
    #[test]
    fn subsidized_sweep_reports_shortfall_when_wallet_cannot_cover() {
        let h = harness(0, BroadcastOutcome::Accepted);
        crate::wallet::test_support::fund_confirmed(&h.wallet, 500);
        set_sweep_rate(&h, 2.0);

        h.engine
            .track_spendable_outputs(&near_dust_batch(&h.keys_manager), None)
            .unwrap();
        let summary = block_on(h.engine.sweep_once());
        assert_eq!(summary.swept, 0);
        let info = h.store.pending_info().unwrap();
        assert!(info.needs_onchain_funds);
        assert_eq!(info.shortfall_sats, Some(80), "580 needed - 500 available");
        assert!(
            h.store.needs_onchain_funds(),
            "gates the 60 s retry cadence"
        );
    }

    /// U11 scenario (reserve untouched): with every confirmed sat reserved
    /// for anchor CPFP the subsidized sweep must NOT spend the wallet UTXO —
    /// shortfall instead.
    #[test]
    fn subsidized_sweep_never_touches_the_anchor_reserve() {
        let h = harness(50_000, BroadcastOutcome::Accepted);
        crate::wallet::test_support::fund_confirmed(&h.wallet, 50_000);
        set_sweep_rate(&h, 2.0);
        let (wallet_outpoint, _) = h.wallet.confirmed_utxos()[0];

        h.engine
            .track_spendable_outputs(&near_dust_batch(&h.keys_manager), None)
            .unwrap();
        let summary = block_on(h.engine.sweep_once());
        assert_eq!(summary.swept, 0);
        assert_eq!(h.broadcast.broadcast_count(), 0);
        assert!(
            h.wallet.owns_unspent_outpoint(&wallet_outpoint),
            "the reserve UTXO is untouched"
        );
        assert!(h.store.pending_info().unwrap().needs_onchain_funds);
    }

    /// U11 scenario (546 dust gate): an LDK floor output between LDK's
    /// internal 294-sat p2wpkh dust and the 546-sat relay gate is refused
    /// BEFORE any signature is spent on it.
    #[test]
    fn subsidized_sweep_refuses_a_sub_546_ldk_output() {
        let h = harness(0, BroadcastOutcome::Accepted);
        crate::wallet::test_support::fund_confirmed(&h.wallet, 50_000);
        set_sweep_rate(&h, 2.0);

        // 5 x 150 = 750: floor change 750-382 = 368 — above LDK's 294,
        // below the 546 relay gate.
        let outputs: Vec<SpendableOutputDescriptor> = (0..5)
            .map(|i| static_output(signable_spk(&h.keys_manager), 150, 0x20 + i, None))
            .collect();
        h.engine.track_spendable_outputs(&outputs, None).unwrap();
        let summary = block_on(h.engine.sweep_once());
        assert_eq!(summary.swept, 0);
        assert_eq!(h.broadcast.broadcast_count(), 0);
        assert!(h.store.pending_info().unwrap().last_attempt_failed);
    }

    /// U11 GUARD 2 (sentinel + verify for shared-input txs): a sentinel
    /// broadcast outcome ("-25"-class) on a SUBSIDIZED tx deletes
    /// descriptors ONLY after the chain view confirms it knows the tx — a
    /// concurrently spent wallet input produces the same error while the
    /// funds never moved.
    #[test]
    fn sentinel_on_a_shared_input_tx_requires_chain_verification() {
        // Unverifiable sentinel: descriptors KEPT.
        let h = harness(0, BroadcastOutcome::AlreadyKnown);
        crate::wallet::test_support::fund_confirmed(&h.wallet, 50_000);
        set_sweep_rate(&h, 2.0);
        h.engine
            .track_spendable_outputs(&near_dust_batch(&h.keys_manager), None)
            .unwrap();
        let summary = block_on(h.engine.sweep_once());
        assert_eq!(summary.swept, 0, "ambiguous sentinel must not delete");
        assert!(h.store.pending_info().is_some(), "descriptors kept");

        // Verified sentinel: the chain knows the tx — descriptors deleted.
        let h = harness(0, BroadcastOutcome::AlreadyKnown);
        h.broadcast.known.store(true, Ordering::Release);
        crate::wallet::test_support::fund_confirmed(&h.wallet, 50_000);
        set_sweep_rate(&h, 2.0);
        h.engine
            .track_spendable_outputs(&near_dust_batch(&h.keys_manager), None)
            .unwrap();
        let summary = block_on(h.engine.sweep_once());
        assert_eq!(summary.swept, 5);
        assert!(h.store.pending_info().is_none());
    }

    /// U11 fee-sanity wiring (the 30x overpay class): with the sanity
    /// ceiling forced below the sweep's effective rate, the built sweep is
    /// REFUSED before broadcast and the descriptors stay.
    #[test]
    fn fee_sanity_refuses_an_overpriced_sweep_before_broadcast() {
        let h = harness(0, BroadcastOutcome::Accepted);
        // 3-block answers its 2,500 sat/kW floor -> ceiling 12,500. A
        // 500 sat/vB sweep rate (the clamp maximum) prices the sweep tx at
        // 125,000 sat/kW — a 50x overpay against the fresh estimate.
        set_sweep_rate(&h, 500.0);
        h.engine
            .track_spendable_outputs(
                &[static_output(
                    signable_spk(&h.keys_manager),
                    1_000_000,
                    1,
                    None,
                )],
                None,
            )
            .unwrap();

        let summary = block_on(h.engine.sweep_once());
        assert_eq!(summary.swept, 0);
        assert_eq!(h.broadcast.broadcast_count(), 0, "refused BEFORE broadcast");
        let info = h.store.pending_info().expect("descriptors kept for retry");
        assert!(info.last_attempt_failed);
    }
}
