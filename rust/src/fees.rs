//! Cached fee estimation (U12/KTD-9): floors, block targets, defaults, clamp
//! ceiling, cache TTL, and failure backoff all mirror the PWA's fee estimator
//! (`src/ldk/traits/fee-estimator.ts` + `src/shared/fee-cache.ts`). Answers
//! every [`ConfirmationTarget`] variant from a cache refreshed off the
//! Esplora fee-estimates endpoint by a background task; the per-variant
//! formula is `max(min(sat_per_vb * 250, 500_000), per_variant_floor, 253)`.

use std::collections::HashMap;
use std::sync::RwLock;
use std::time::{Duration, Instant};

use lightning::chain::chaininterface::{
    ConfirmationTarget, FeeEstimator, FEERATE_FLOOR_SATS_PER_KW,
};

/// Cache freshness window: a refresh is due once the last successful fetch is
/// older than this (PWA `CACHE_TTL_MS`).
pub(crate) const FEE_CACHE_TTL: Duration = Duration::from_secs(60);

/// Minimum wait after a failed refresh before trying again (PWA
/// `FAILURE_BACKOFF_MS`), so an unreachable endpoint isn't hammered.
pub(crate) const FEE_REFRESH_BACKOFF: Duration = Duration::from_secs(15);

/// Clamp ceiling: ~2,000 sat/vB — beyond this, something is wrong (PWA
/// `MAX_FEE_SAT_KW`). Applied before the per-variant floor.
pub(crate) const MAX_FEE_RATE_SAT_PER_KW: u32 = 500_000;

/// The on-chain send path's block target (U8/KTD-9, PWA `context.tsx`
/// `FEE_TARGET_BLOCKS`): the raw 6-block Esplora estimate, not an LDK
/// confirmation-target slot.
pub(crate) const ONCHAIN_SEND_TARGET_BLOCKS: u16 = 6;

/// Minimum fee rate the wallet broadcasts an on-chain send at (U8, PWA
/// `MIN_FEE_RATE_SAT_VB`, `config.ts:6`).
pub(crate) const MIN_ONCHAIN_SEND_RATE_SAT_PER_VB: u64 = 2;

/// The raw 6-block sat/vB out of an Esplora fee-estimates response for the
/// U8 send path, filtered like the PWA's fee cache (finite, positive) —
/// `None` when the response has no usable 6-block row.
pub(crate) fn onchain_send_sat_per_vb_from_estimates(estimates: &HashMap<u16, f64>) -> Option<f64> {
    estimates
        .get(&ONCHAIN_SEND_TARGET_BLOCKS)
        .copied()
        .filter(|rate| rate.is_finite() && *rate > 0.0)
}

/// One row per `ConfirmationTarget` variant, in cache-slot order:
/// `(target, num_blocks, floor_sat_per_kw)` — the PWA's `targetToBlocks` and
/// `DEFAULT_FEE_RATES` tables verbatim. Notably `UrgentOnChainSweep` targets
/// 3 blocks, not 1 (KTD-9: the 1-block default overpaid 30x in a real
/// incident), and `MaximumFeeEstimate` floors at 50_000 sat/kw.
/// [`target_index`] stays an exhaustive `match`, so a new LDK variant still
/// breaks the build, which is the point.
const TARGET_TABLE: [(ConfirmationTarget, usize, u32); 8] = [
    (ConfirmationTarget::MaximumFeeEstimate, 1, 50_000),
    (ConfirmationTarget::UrgentOnChainSweep, 3, 2_500),
    (
        ConfirmationTarget::MinAllowedAnchorChannelRemoteFee,
        144,
        FEERATE_FLOOR_SATS_PER_KW,
    ),
    (
        ConfirmationTarget::MinAllowedNonAnchorChannelRemoteFee,
        144,
        FEERATE_FLOOR_SATS_PER_KW,
    ),
    (ConfirmationTarget::AnchorChannelFee, 6, 2_500),
    (ConfirmationTarget::NonAnchorChannelFee, 6, 5_000),
    (ConfirmationTarget::ChannelCloseMinimum, 12, 1_000),
    (ConfirmationTarget::OutputSpendingFee, 12, 5_000),
];

/// Default sat/vB per block target, used until the first successful refresh
/// (PWA `DEFAULT_RATES`); block targets without a row fall back to the
/// 6-block default, exactly like the PWA's `defaultRate`.
const DEFAULT_SAT_PER_VB: [(usize, f64); 4] = [(1, 10.0), (6, 5.0), (12, 3.0), (144, 2.0)];

/// Every `ConfirmationTarget` variant, for tests (cache refreshes iterate
/// [`TARGET_TABLE`] directly).
#[cfg(test)]
pub(crate) const ALL_CONFIRMATION_TARGETS: [ConfirmationTarget; 8] = {
    let mut targets = [TARGET_TABLE[0].0; 8];
    let mut i = 0;
    while i < targets.len() {
        targets[i] = TARGET_TABLE[i].0;
        i += 1;
    }
    targets
};

/// Stable index for each target ([`TARGET_TABLE`] row and cache slot). The
/// exhaustive `match` fails to compile when LDK adds a variant.
fn target_index(target: ConfirmationTarget) -> usize {
    match target {
        ConfirmationTarget::MaximumFeeEstimate => 0,
        ConfirmationTarget::UrgentOnChainSweep => 1,
        ConfirmationTarget::MinAllowedAnchorChannelRemoteFee => 2,
        ConfirmationTarget::MinAllowedNonAnchorChannelRemoteFee => 3,
        ConfirmationTarget::AnchorChannelFee => 4,
        ConfirmationTarget::NonAnchorChannelFee => 5,
        ConfirmationTarget::ChannelCloseMinimum => 6,
        ConfirmationTarget::OutputSpendingFee => 7,
    }
}

/// The PWA's `computeFeeRateSatKw` core: convert sat/vB to sat/kw, clamp at
/// the ceiling, then enforce the per-variant floor and LDK's 253 minimum.
fn clamp_and_floor(sat_per_vb: f64, floor_sat_per_kw: u32) -> u32 {
    let sat_per_kw = ((sat_per_vb * 250.0).round() as u64).min(MAX_FEE_RATE_SAT_PER_KW as u64);
    (sat_per_kw as u32)
        .max(floor_sat_per_kw)
        .max(FEERATE_FLOOR_SATS_PER_KW)
}

/// Default sat/vB for a block target (PWA `defaultRate`): exact row, else the
/// 6-block default, else 1 sat/vB.
fn default_sat_per_vb(num_blocks: usize) -> f64 {
    DEFAULT_SAT_PER_VB
        .iter()
        .find(|(blocks, _)| *blocks == num_blocks)
        .or_else(|| DEFAULT_SAT_PER_VB.iter().find(|(blocks, _)| *blocks == 6))
        .map_or(1.0, |(_, rate)| *rate)
}

/// Static fallback (sat/kw) used until the first successful refresh: the
/// default-rate table put through the same clamp-then-floor formula, so an
/// offline start answers exactly what the PWA would.
fn fallback_sat_per_kw(target: ConfirmationTarget) -> u32 {
    let (_, num_blocks, floor) = TARGET_TABLE[target_index(target)];
    clamp_and_floor(default_sat_per_vb(num_blocks), floor)
}

/// Pure translation of an Esplora fee-estimates response (`blocks -> sat/vB`)
/// into a per-target cache in sat/kw. Kept free of I/O so the table, clamp,
/// and floors are unit-testable offline.
pub(crate) fn cache_from_esplora_estimates(estimates: &HashMap<u16, f64>) -> [u32; 8] {
    // Sort once and answer every target from the sorted view; picking the
    // largest block count at or below the target replicates
    // `esplora_client::convert_fee_rate` (including no-match -> None).
    let mut sorted: Vec<(u16, f64)> = estimates.iter().map(|(k, v)| (*k, *v)).collect();
    sorted.sort_unstable_by_key(|(blocks, _)| *blocks);

    let mut cache = [0u32; 8];
    for (index, (_, num_blocks, floor)) in TARGET_TABLE.into_iter().enumerate() {
        let split = sorted.partition_point(|(blocks, _)| (*blocks as usize) <= num_blocks);
        // Fall back to 1 sat/vB if the endpoint returned nothing usable for
        // this target; the floor below keeps the result relay-viable.
        let sat_per_vb = split
            .checked_sub(1)
            .map(|i| sorted[i].1)
            .map_or(1.0, |rate| rate.max(1.0));
        cache[index] = clamp_and_floor(sat_per_vb, floor);
    }
    cache
}

/// Refresh bookkeeping for the TTL/backoff policy, separate from the answer
/// path so [`FeeEstimator::get_est_sat_per_1000_weight`] stays lock-cheap.
struct RefreshState {
    fetched_at: Option<Instant>,
    failed_at: Option<Instant>,
}

/// Cached [`FeeEstimator`]: never hits the network on the answer path. The
/// background task consults [`CachedFeeEstimator::needs_refresh`] (60 s TTL,
/// 15 s failure backoff — U12/KTD-9) and reports outcomes back via
/// [`CachedFeeEstimator::set_cache`] / [`CachedFeeEstimator::record_failure`].
pub(crate) struct CachedFeeEstimator {
    /// `None` per slot until the first successful refresh.
    cache: RwLock<[Option<u32>; 8]>,
    /// Raw 6-block sat/vB for the U8 on-chain send path; `None` until a
    /// refresh delivers a usable 6-block row (the PWA's shared fee cache
    /// keeps the raw Esplora format for exactly this consumer).
    onchain_send_sat_per_vb: RwLock<Option<f64>>,
    refresh: RwLock<RefreshState>,
}

impl CachedFeeEstimator {
    pub(crate) fn new() -> Self {
        Self {
            cache: RwLock::new([None; 8]),
            onchain_send_sat_per_vb: RwLock::new(None),
            refresh: RwLock::new(RefreshState {
                fetched_at: None,
                failed_at: None,
            }),
        }
    }

    /// Installs the raw 6-block sat/vB alongside a cache refresh (U8).
    pub(crate) fn set_onchain_send_rate(&self, sat_per_vb: Option<f64>) {
        *self.onchain_send_sat_per_vb.write().unwrap() = sat_per_vb;
    }

    /// The U8 send path's fee rate in sat/vB (KTD-9, PWA `getFeeRate` +
    /// `context.tsx:32-36`): the cached 6-block estimate — or the PWA's
    /// 6-block default (5 sat/vB) before the first refresh — rounded UP and
    /// clamped to at least [`MIN_ONCHAIN_SEND_RATE_SAT_PER_VB`].
    pub(crate) fn onchain_send_rate_sat_per_vb(&self) -> u64 {
        let raw = self
            .onchain_send_sat_per_vb
            .read()
            .unwrap()
            .unwrap_or_else(|| default_sat_per_vb(ONCHAIN_SEND_TARGET_BLOCKS as usize));
        (raw.ceil() as u64).max(MIN_ONCHAIN_SEND_RATE_SAT_PER_VB)
    }

    /// Installs a refreshed cache produced by [`cache_from_esplora_estimates`]
    /// and marks the cache fresh as of `now`.
    pub(crate) fn set_cache(&self, rates: [u32; 8]) {
        self.set_cache_at(rates, Instant::now());
    }

    fn set_cache_at(&self, rates: [u32; 8], now: Instant) {
        {
            let mut locked = self.cache.write().unwrap();
            for (slot, rate) in locked.iter_mut().zip(rates) {
                *slot = Some(rate);
            }
        }
        let mut refresh = self.refresh.write().unwrap();
        refresh.fetched_at = Some(now);
        refresh.failed_at = None;
    }

    /// Records a failed refresh attempt; the next attempt waits out
    /// [`FEE_REFRESH_BACKOFF`].
    pub(crate) fn record_failure(&self) {
        self.record_failure_at(Instant::now());
    }

    fn record_failure_at(&self, now: Instant) {
        self.refresh.write().unwrap().failed_at = Some(now);
    }

    /// Whether a refresh is due: the cache is stale (no fetch yet, or the
    /// last one is older than [`FEE_CACHE_TTL`]) AND the last failure (if
    /// any) is at least [`FEE_REFRESH_BACKOFF`] ago.
    pub(crate) fn needs_refresh(&self) -> bool {
        self.needs_refresh_at(Instant::now())
    }

    fn needs_refresh_at(&self, now: Instant) -> bool {
        let refresh = self.refresh.read().unwrap();
        let stale = refresh
            .fetched_at
            .is_none_or(|fetched_at| now.duration_since(fetched_at) > FEE_CACHE_TTL);
        let backed_off = refresh
            .failed_at
            .is_none_or(|failed_at| now.duration_since(failed_at) >= FEE_REFRESH_BACKOFF);
        stale && backed_off
    }
}

impl FeeEstimator for CachedFeeEstimator {
    fn get_est_sat_per_1000_weight(&self, confirmation_target: ConfirmationTarget) -> u32 {
        let cached = self.cache.read().unwrap()[target_index(confirmation_target)];
        cached
            .unwrap_or_else(|| fallback_sat_per_kw(confirmation_target))
            .max(FEERATE_FLOOR_SATS_PER_KW)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_target_is_floored_when_endpoint_returns_empty_data() {
        let estimator = CachedFeeEstimator::new();
        estimator.set_cache(cache_from_esplora_estimates(&HashMap::new()));
        for target in ALL_CONFIRMATION_TARGETS {
            assert!(
                estimator.get_est_sat_per_1000_weight(target) >= FEERATE_FLOOR_SATS_PER_KW,
                "{target:?} fell below the 253 sat/kw floor on empty estimates"
            );
        }
    }

    #[test]
    fn every_target_is_floored_when_endpoint_returns_absurdly_low_data() {
        let estimator = CachedFeeEstimator::new();
        let low: HashMap<u16, f64> = [(1u16, 0.01), (144u16, 0.0), (1008u16, 0.001)]
            .into_iter()
            .collect();
        estimator.set_cache(cache_from_esplora_estimates(&low));
        for target in ALL_CONFIRMATION_TARGETS {
            assert!(
                estimator.get_est_sat_per_1000_weight(target) >= FEERATE_FLOOR_SATS_PER_KW,
                "{target:?} fell below the 253 sat/kw floor on low estimates"
            );
        }
    }

    #[test]
    fn every_target_answers_from_the_static_fallback_before_first_refresh() {
        let estimator = CachedFeeEstimator::new();
        for target in ALL_CONFIRMATION_TARGETS {
            let rate = estimator.get_est_sat_per_1000_weight(target);
            assert!(
                rate >= FEERATE_FLOOR_SATS_PER_KW,
                "{target:?} fallback below floor"
            );
            assert_eq!(
                rate,
                fallback_sat_per_kw(target).max(FEERATE_FLOOR_SATS_PER_KW)
            );
        }
    }

    /// U12/KTD-9: the per-variant block targets and floors must match the
    /// PWA's fee-estimator table (`src/ldk/traits/fee-estimator.ts`) exactly.
    /// Estimates are chosen so every block target maps to a distinct rate.
    #[test]
    fn fee_table_matches_pwa_floors_and_targets() {
        let estimates: HashMap<u16, f64> = [
            (1u16, 400.0),
            (3u16, 100.0),
            (6u16, 50.0),
            (12u16, 20.0),
            (144u16, 4.0),
        ]
        .into_iter()
        .collect();
        let estimator = CachedFeeEstimator::new();
        estimator.set_cache(cache_from_esplora_estimates(&estimates));

        // (target, expected sat/kw) — PWA formula: min(sat_vb*250, 500_000)
        // then max(per-variant floor, 253); no ldk-node post-adjustments.
        let expected = [
            // 1 block: 400 sat/vB -> 100_000, floor 50_000.
            (ConfirmationTarget::MaximumFeeEstimate, 100_000),
            // 3 blocks (KTD-9 incident learning): 100 sat/vB -> 25_000.
            (ConfirmationTarget::UrgentOnChainSweep, 25_000),
            // 144 blocks: 4 sat/vB -> 1_000, floor 253.
            (ConfirmationTarget::MinAllowedAnchorChannelRemoteFee, 1_000),
            (
                ConfirmationTarget::MinAllowedNonAnchorChannelRemoteFee,
                1_000,
            ),
            // 6 blocks: 50 sat/vB -> 12_500.
            (ConfirmationTarget::AnchorChannelFee, 12_500),
            (ConfirmationTarget::NonAnchorChannelFee, 12_500),
            // 12 blocks: 20 sat/vB -> 5_000.
            (ConfirmationTarget::ChannelCloseMinimum, 5_000),
            (ConfirmationTarget::OutputSpendingFee, 5_000),
        ];
        for (target, expected_sat_per_kw) in expected {
            assert_eq!(
                estimator.get_est_sat_per_1000_weight(target),
                expected_sat_per_kw,
                "{target:?} diverged from the PWA fee table"
            );
        }
    }

    /// U12/KTD-9: when the estimates come in absurdly low, every variant
    /// answers its PWA floor, not the shared 253 minimum.
    #[test]
    fn every_target_floors_at_its_pwa_value_on_low_estimates() {
        let low: HashMap<u16, f64> = [(1u16, 1.0), (144u16, 1.0)].into_iter().collect();
        let estimator = CachedFeeEstimator::new();
        estimator.set_cache(cache_from_esplora_estimates(&low));
        let expected = [
            (ConfirmationTarget::MaximumFeeEstimate, 50_000),
            (ConfirmationTarget::UrgentOnChainSweep, 2_500),
            (ConfirmationTarget::MinAllowedAnchorChannelRemoteFee, 253),
            (ConfirmationTarget::MinAllowedNonAnchorChannelRemoteFee, 253),
            (ConfirmationTarget::AnchorChannelFee, 2_500),
            (ConfirmationTarget::NonAnchorChannelFee, 5_000),
            (ConfirmationTarget::ChannelCloseMinimum, 1_000),
            (ConfirmationTarget::OutputSpendingFee, 5_000),
        ];
        for (target, floor) in expected {
            assert_eq!(
                estimator.get_est_sat_per_1000_weight(target),
                floor,
                "{target:?} did not floor at its PWA value"
            );
        }
    }

    /// U12/KTD-9: absurd estimates clamp at the PWA's 500_000 sat/kw ceiling.
    #[test]
    fn absurd_estimates_clamp_at_the_pwa_ceiling() {
        let absurd: HashMap<u16, f64> =
            [(1u16, 10_000.0), (144u16, 10_000.0)].into_iter().collect();
        let estimator = CachedFeeEstimator::new();
        estimator.set_cache(cache_from_esplora_estimates(&absurd));
        for target in ALL_CONFIRMATION_TARGETS {
            assert_eq!(
                estimator.get_est_sat_per_1000_weight(target),
                500_000,
                "{target:?} exceeded the 500_000 sat/kw clamp"
            );
        }
    }

    /// U12/KTD-9: before the first refresh, every variant answers from the
    /// PWA's default sat/vB table {1:10, 6:5, 12:3, 144:2} put through the
    /// same clamp-then-floor formula.
    #[test]
    fn offline_defaults_mirror_the_pwa_default_rate_table() {
        let estimator = CachedFeeEstimator::new();
        let expected = [
            // 10 sat/vB -> 2_500, floor 50_000.
            (ConfirmationTarget::MaximumFeeEstimate, 50_000),
            // 3 blocks has no default row; the PWA falls back to the 6-block
            // default (5 sat/vB -> 1_250), floor 2_500.
            (ConfirmationTarget::UrgentOnChainSweep, 2_500),
            // 2 sat/vB -> 500, floor 253.
            (ConfirmationTarget::MinAllowedAnchorChannelRemoteFee, 500),
            (ConfirmationTarget::MinAllowedNonAnchorChannelRemoteFee, 500),
            // 5 sat/vB -> 1_250, floor 2_500 / 5_000.
            (ConfirmationTarget::AnchorChannelFee, 2_500),
            (ConfirmationTarget::NonAnchorChannelFee, 5_000),
            // 3 sat/vB -> 750, floor 1_000 / 5_000.
            (ConfirmationTarget::ChannelCloseMinimum, 1_000),
            (ConfirmationTarget::OutputSpendingFee, 5_000),
        ];
        for (target, expected_sat_per_kw) in expected {
            assert_eq!(
                estimator.get_est_sat_per_1000_weight(target),
                expected_sat_per_kw,
                "{target:?} default diverged from the PWA table"
            );
        }
    }

    /// Block-target lookup picks the largest estimate at or below the target
    /// (esplora `convert_fee_rate` semantics): 3 blocks with no 3-block row
    /// answers from the 1-block estimate, never a cheaper later one.
    #[test]
    fn sane_estimates_map_to_the_right_block_targets() {
        let estimates: HashMap<u16, f64> =
            [(1u16, 40.0), (6u16, 30.0), (144u16, 5.0), (1008u16, 4.0)]
                .into_iter()
                .collect();
        let estimator = CachedFeeEstimator::new();
        estimator.set_cache(cache_from_esplora_estimates(&estimates));
        // NonAnchorChannelFee targets 6 blocks: 30 sat/vB * 250 = 7_500.
        assert_eq!(
            estimator.get_est_sat_per_1000_weight(ConfirmationTarget::NonAnchorChannelFee),
            7_500
        );
        // UrgentOnChainSweep targets 3 blocks; the closest estimate at or
        // below 3 blocks is the 1-block one: 40 sat/vB * 250 = 10_000.
        assert_eq!(
            estimator.get_est_sat_per_1000_weight(ConfirmationTarget::UrgentOnChainSweep),
            10_000
        );
        // OutputSpendingFee targets 12 blocks; closest at or below is the
        // 6-block estimate: 7_500 (above its 5_000 floor).
        assert_eq!(
            estimator.get_est_sat_per_1000_weight(ConfirmationTarget::OutputSpendingFee),
            7_500
        );
    }

    /// U8/KTD-9: the on-chain send rate is the raw 6-block estimate, ceil'd
    /// and clamped >= 2 sat/vB; before any refresh it answers the PWA's
    /// 6-block default (5 sat/vB); a response without a usable 6-block row
    /// falls back to the default too.
    #[test]
    fn onchain_send_rate_mirrors_the_pwa_six_block_path() {
        let estimator = CachedFeeEstimator::new();
        // Before the first refresh: the PWA 6-block default.
        assert_eq!(estimator.onchain_send_rate_sat_per_vb(), 5);

        // Fractional rates round UP (PWA Math.ceil).
        let estimates: HashMap<u16, f64> = [(6u16, 7.2)].into_iter().collect();
        estimator.set_onchain_send_rate(onchain_send_sat_per_vb_from_estimates(&estimates));
        assert_eq!(estimator.onchain_send_rate_sat_per_vb(), 8);

        // The 2 sat/vB floor (PWA MIN_FEE_RATE_SAT_VB).
        let low: HashMap<u16, f64> = [(6u16, 0.4)].into_iter().collect();
        estimator.set_onchain_send_rate(onchain_send_sat_per_vb_from_estimates(&low));
        assert_eq!(estimator.onchain_send_rate_sat_per_vb(), 2);

        // No usable 6-block row (missing / non-positive): default 5.
        let missing: HashMap<u16, f64> = [(1u16, 30.0)].into_iter().collect();
        estimator.set_onchain_send_rate(onchain_send_sat_per_vb_from_estimates(&missing));
        assert_eq!(estimator.onchain_send_rate_sat_per_vb(), 5);
        let broken: HashMap<u16, f64> = [(6u16, f64::NAN)].into_iter().collect();
        estimator.set_onchain_send_rate(onchain_send_sat_per_vb_from_estimates(&broken));
        assert_eq!(estimator.onchain_send_rate_sat_per_vb(), 5);
    }

    /// U12/KTD-9: the 60 s TTL — fresh right after a refresh, due again once
    /// the TTL has elapsed.
    #[test]
    fn cache_ttl_is_honored() {
        let estimator = CachedFeeEstimator::new();
        let t0 = Instant::now();
        assert!(
            estimator.needs_refresh_at(t0),
            "a never-refreshed cache is due immediately"
        );

        estimator.set_cache_at(cache_from_esplora_estimates(&HashMap::new()), t0);
        assert!(
            !estimator.needs_refresh_at(t0 + FEE_CACHE_TTL - Duration::from_secs(1)),
            "within the TTL the cache is fresh"
        );
        assert!(
            estimator.needs_refresh_at(t0 + FEE_CACHE_TTL + Duration::from_secs(1)),
            "past the TTL a refresh is due"
        );
    }

    /// U12/KTD-9: the 15 s failure backoff — a failed refresh suppresses
    /// retries until the backoff elapses, and a later success clears it.
    #[test]
    fn failure_backoff_is_honored() {
        let estimator = CachedFeeEstimator::new();
        let t0 = Instant::now();
        estimator.record_failure_at(t0);
        assert!(
            !estimator.needs_refresh_at(t0 + FEE_REFRESH_BACKOFF - Duration::from_secs(1)),
            "within the backoff no retry is due"
        );
        assert!(
            estimator.needs_refresh_at(t0 + FEE_REFRESH_BACKOFF),
            "after the backoff the retry is due"
        );

        // A success both installs the cache and clears the failure mark.
        estimator.set_cache_at(
            cache_from_esplora_estimates(&HashMap::new()),
            t0 + FEE_REFRESH_BACKOFF,
        );
        assert!(!estimator.needs_refresh_at(t0 + FEE_REFRESH_BACKOFF + Duration::from_secs(1)));
    }
}
