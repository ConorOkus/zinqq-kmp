//! The KTD-9 skim guard at claim time: [`ClaimTracker`] tracks the opening
//! fee agreed per JIT invoice and decides claim-or-fail for each
//! `PaymentClaimable` (the LSP must not take more than agreed).

use std::collections::HashMap;
use std::sync::Mutex;

use lightning::types::payment::{PaymentHash, PaymentPreimage};

/// What to do with a `PaymentClaimable` event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ClaimDecision {
    /// Claim with this preimage (`claim_funds` is idempotent in LDK, so a
    /// replayed claimable simply claims again).
    Claim(PaymentPreimage),
    /// Fail the HTLC back with this reason.
    FailBack(String),
}

/// Skim bookkeeping across the claim lifecycle. At invoice creation the
/// agreed opening fee is registered per payment hash; at `PaymentClaimable`
/// the observed `counterparty_skimmed_fee_msat` is checked against it (the
/// ldk-node guard: the LSP must not take more than agreed) and recorded; at
/// `PaymentClaimed` the recorded skim is consumed for the public
/// `PaymentReceived` event.
///
/// In-memory only: after a process restart a replayed claimable for a JIT
/// invoice from a previous session has no registered fee and a nonzero skim
/// is refused (HTLC failed back; the payer retries). Acceptable for the
/// foreground-only spike flow.
#[derive(Default)]
pub(crate) struct ClaimTracker {
    claims: Mutex<HashMap<PaymentHash, ClaimState>>,
}

/// Per-payment-hash skim bookkeeping.
#[derive(Default)]
struct ClaimState {
    /// Max skim agreed at invoice creation.
    expected_fee_msat: Option<u64>,
    /// Skim observed on the (latest) claimable event.
    observed_skim_msat: Option<u64>,
}

impl ClaimTracker {
    /// Registers the opening fee agreed for a JIT invoice.
    pub(crate) fn register_expected_fee(&self, payment_hash: PaymentHash, fee_msat: u64) {
        self.claims
            .lock()
            .unwrap()
            .entry(payment_hash)
            .or_default()
            .expected_fee_msat = Some(fee_msat);
    }

    /// Decides claim-or-fail for a claimable payment. Idempotent: replaying
    /// the same claimable yields the same decision and never panics.
    pub(crate) fn decide(
        &self,
        payment_hash: PaymentHash,
        skimmed_fee_msat: u64,
        preimage: Option<PaymentPreimage>,
    ) -> ClaimDecision {
        let mut claims = self.claims.lock().unwrap();
        let state = claims.entry(payment_hash).or_default();
        state.observed_skim_msat = Some(skimmed_fee_msat);

        let Some(preimage) = preimage else {
            return ClaimDecision::FailBack(
                "claimable payment carries no preimage (not created via create_inbound_payment)"
                    .to_string(),
            );
        };

        // ldk-node's guard: never let the counterparty skim more than the
        // agreed opening fee (payments we never sold a JIT channel for get an
        // allowance of zero).
        let max_skim_msat = state.expected_fee_msat.unwrap_or(0);
        if skimmed_fee_msat > max_skim_msat {
            return ClaimDecision::FailBack(format!(
                "counterparty skimmed {skimmed_fee_msat}msat, more than the agreed \
                 {max_skim_msat}msat opening fee"
            ));
        }

        ClaimDecision::Claim(preimage)
    }

    /// Consumes the recorded skim when the payment is durably claimed.
    pub(crate) fn take_skim(&self, payment_hash: &PaymentHash) -> Option<u64> {
        self.claims
            .lock()
            .unwrap()
            .remove(payment_hash)
            .and_then(|state| state.observed_skim_msat)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---------- claim tracker: guard + idempotency ----------

    #[test]
    fn claim_decision_claims_when_skim_is_within_the_agreed_fee() {
        let tracker = ClaimTracker::default();
        let hash = PaymentHash([1u8; 32]);
        let preimage = PaymentPreimage([2u8; 32]);
        tracker.register_expected_fee(hash, 5_000);

        assert_eq!(
            tracker.decide(hash, 5_000, Some(preimage)),
            ClaimDecision::Claim(preimage)
        );
        assert_eq!(tracker.take_skim(&hash), Some(5_000));
        assert_eq!(tracker.take_skim(&hash), None, "skim is consumed once");
    }

    #[test]
    fn replayed_claimable_after_unacked_claim_is_tolerated() {
        // KTD idempotency scenario: a crash between claim and ack replays the
        // claimable; the handler must decide identically and not panic.
        let tracker = ClaimTracker::default();
        let hash = PaymentHash([3u8; 32]);
        let preimage = PaymentPreimage([4u8; 32]);
        tracker.register_expected_fee(hash, 1_000);

        let first = tracker.decide(hash, 1_000, Some(preimage));
        let replay = tracker.decide(hash, 1_000, Some(preimage));
        assert_eq!(first, replay);
        assert_eq!(first, ClaimDecision::Claim(preimage));
    }

    #[test]
    fn overskimming_lsp_is_failed_back_not_claimed() {
        let tracker = ClaimTracker::default();
        let hash = PaymentHash([5u8; 32]);
        tracker.register_expected_fee(hash, 1_000);
        assert!(matches!(
            tracker.decide(hash, 1_001, Some(PaymentPreimage([6u8; 32]))),
            ClaimDecision::FailBack(_)
        ));

        // Unknown payment hash: zero skim allowance.
        let unknown = PaymentHash([7u8; 32]);
        assert!(matches!(
            tracker.decide(unknown, 1, Some(PaymentPreimage([6u8; 32]))),
            ClaimDecision::FailBack(_)
        ));
        assert_eq!(
            tracker.decide(unknown, 0, Some(PaymentPreimage([6u8; 32]))),
            ClaimDecision::Claim(PaymentPreimage([6u8; 32]))
        );
    }

    #[test]
    fn missing_preimage_is_failed_back() {
        let tracker = ClaimTracker::default();
        assert!(matches!(
            tracker.decide(PaymentHash([8u8; 32]), 0, None),
            ClaimDecision::FailBack(_)
        ));
    }
}
