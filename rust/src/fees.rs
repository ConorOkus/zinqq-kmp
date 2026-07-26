//! Cached fee estimation. Answers every [`ConfirmationTarget`] variant from a
//! cache refreshed off the Esplora fee-estimates endpoint by a background
//! task, with a static fallback table for offline starts. Every answer is
//! floored at [`FEERATE_FLOOR_SATS_PER_KW`] (253 sat/kw).

use std::collections::HashMap;
use std::sync::RwLock;

use lightning::chain::chaininterface::{
    ConfirmationTarget, FeeEstimator, FEERATE_FLOOR_SATS_PER_KW,
};

/// Every `ConfirmationTarget` variant, for cache refreshes and tests. If LDK
/// adds a variant, the exhaustive `match`es below fail to compile, which is
/// the point.
pub(crate) const ALL_CONFIRMATION_TARGETS: [ConfirmationTarget; 8] = [
    ConfirmationTarget::MaximumFeeEstimate,
    ConfirmationTarget::UrgentOnChainSweep,
    ConfirmationTarget::MinAllowedAnchorChannelRemoteFee,
    ConfirmationTarget::MinAllowedNonAnchorChannelRemoteFee,
    ConfirmationTarget::AnchorChannelFee,
    ConfirmationTarget::NonAnchorChannelFee,
    ConfirmationTarget::ChannelCloseMinimum,
    ConfirmationTarget::OutputSpendingFee,
];

/// Stable index for each target so the cache is a plain array.
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

/// The block target each variant is estimated at (mirrors ldk-node).
fn num_blocks_for_target(target: ConfirmationTarget) -> usize {
    match target {
        ConfirmationTarget::MaximumFeeEstimate => 1,
        ConfirmationTarget::UrgentOnChainSweep => 6,
        ConfirmationTarget::MinAllowedAnchorChannelRemoteFee => 1008,
        ConfirmationTarget::MinAllowedNonAnchorChannelRemoteFee => 144,
        ConfirmationTarget::AnchorChannelFee => 1008,
        ConfirmationTarget::NonAnchorChannelFee => 12,
        ConfirmationTarget::ChannelCloseMinimum => 144,
        ConfirmationTarget::OutputSpendingFee => 12,
    }
}

/// Static fallback (sat/kw) used until the first successful refresh, so an
/// offline start still answers sanely (mirrors ldk-node's table).
fn fallback_sat_per_kw(target: ConfirmationTarget) -> u32 {
    match target {
        ConfirmationTarget::MaximumFeeEstimate => 8000,
        ConfirmationTarget::UrgentOnChainSweep => 5000,
        ConfirmationTarget::MinAllowedAnchorChannelRemoteFee => FEERATE_FLOOR_SATS_PER_KW,
        ConfirmationTarget::MinAllowedNonAnchorChannelRemoteFee => FEERATE_FLOOR_SATS_PER_KW,
        ConfirmationTarget::AnchorChannelFee => 500,
        ConfirmationTarget::NonAnchorChannelFee => 1000,
        ConfirmationTarget::ChannelCloseMinimum => 500,
        ConfirmationTarget::OutputSpendingFee => 1000,
    }
}

/// Post-estimation adjustments LDK's `ConfirmationTarget` semantics require
/// (mirrors ldk-node `apply_post_estimation_adjustments`).
fn apply_post_estimation_adjustments(target: ConfirmationTarget, sat_per_kw: u64) -> u64 {
    match target {
        ConfirmationTarget::MinAllowedNonAnchorChannelRemoteFee => sat_per_kw
            .saturating_sub(250)
            .max(FEERATE_FLOOR_SATS_PER_KW as u64),
        ConfirmationTarget::MaximumFeeEstimate => sat_per_kw
            .saturating_mul(11)
            .saturating_div(10)
            .saturating_add(2500),
        _ => sat_per_kw,
    }
}

/// Pure translation of an Esplora fee-estimates response (`blocks -> sat/vB`)
/// into a per-target cache in sat/kw. Kept free of I/O so the floor and
/// mapping are unit-testable offline.
pub(crate) fn cache_from_esplora_estimates(estimates: &HashMap<u16, f64>) -> [u32; 8] {
    let mut cache = [0u32; 8];
    for target in ALL_CONFIRMATION_TARGETS {
        let num_blocks = num_blocks_for_target(target);
        // Fall back to 1 sat/vB if the endpoint returned nothing usable for
        // this target; the floor below keeps the result relay-viable.
        let sat_per_vb = esplora_client::convert_fee_rate(num_blocks, estimates.clone())
            .map_or(1.0, |rate| rate.max(1.0));
        let sat_per_kw = (sat_per_vb as f64 * 250.0) as u64;
        let adjusted = apply_post_estimation_adjustments(target, sat_per_kw);
        cache[target_index(target)] =
            u32::try_from(adjusted.max(FEERATE_FLOOR_SATS_PER_KW as u64)).unwrap_or(u32::MAX);
    }
    cache
}

/// Cached [`FeeEstimator`]: never hits the network on the answer path.
pub(crate) struct CachedFeeEstimator {
    /// `None` per slot until the first successful refresh.
    cache: RwLock<[Option<u32>; 8]>,
}

impl CachedFeeEstimator {
    pub(crate) fn new() -> Self {
        Self {
            cache: RwLock::new([None; 8]),
        }
    }

    /// Installs a refreshed cache produced by [`cache_from_esplora_estimates`].
    pub(crate) fn set_cache(&self, rates: [u32; 8]) {
        let mut locked = self.cache.write().unwrap();
        for (slot, rate) in locked.iter_mut().zip(rates) {
            *slot = Some(rate);
        }
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

    #[test]
    fn sane_estimates_map_to_the_right_block_targets() {
        let estimates: HashMap<u16, f64> =
            [(1u16, 40.0), (6u16, 20.0), (144u16, 5.0), (1008u16, 1.0)]
                .into_iter()
                .collect();
        let estimator = CachedFeeEstimator::new();
        estimator.set_cache(cache_from_esplora_estimates(&estimates));
        // NonAnchorChannelFee targets 12 blocks; the closest estimate at or
        // below 12 blocks is the 6-block one: 20 sat/vB * 250 = 5000 sat/kw.
        assert_eq!(
            estimator.get_est_sat_per_1000_weight(ConfirmationTarget::NonAnchorChannelFee),
            5000
        );
        // UrgentOnChainSweep targets 6 blocks: same 20 sat/vB estimate.
        assert_eq!(
            estimator.get_est_sat_per_1000_weight(ConfirmationTarget::UrgentOnChainSweep),
            5000
        );
        // MaximumFeeEstimate targets 1 block with the +10%+2500 adjustment:
        // 40 sat/vB * 250 = 10000 -> 10000 * 1.1 + 2500 = 13500.
        assert_eq!(
            estimator.get_est_sat_per_1000_weight(ConfirmationTarget::MaximumFeeEstimate),
            13500
        );
    }
}
