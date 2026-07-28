package zinqq.app

import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertFalse
import kotlin.test.assertNull
import kotlin.test.assertTrue
import uniffi.wallet_core.Event
import uniffi.wallet_core.RecoveryStatusView

/**
 * Home banner gating (U14, R9):
 * - RecoveryBanner renders whenever recovery state exists (`Home.tsx:80-84`),
 *   with the PWA's two variants (`RecoveryBanner.tsx`); only the
 *   sweep-confirmed variant is dismissible.
 * - PendingSweepBanner is `lastAttemptFailed`-gated (`Home.tsx:86-90`) with
 *   the heading/subtext/deep-link matrix from `PendingSweepBanner.tsx`.
 */
class BannerGatingTest {
    // --- RecoveryBanner ---

    @Test
    fun noRecoveryStateShowsNoBanner() {
        assertNull(recoveryBanner(null, dismissed = false))
    }

    @Test
    fun needsRecoveryBannerNavigatesToRecover() {
        val banner = recoveryBanner(
            recoveryStateView(status = RecoveryStatusView.NEEDS_RECOVERY),
            dismissed = false,
        )!!
        assertEquals("Your funds are safe", banner.title)
        assertEquals("A small deposit is needed to unlock them", banner.subtitle)
        assertTrue(banner.navigatesToRecover)
        assertFalse(banner.dismissible)
    }

    @Test
    fun sweepConfirmedBannerIsDismissible() {
        val banner = recoveryBanner(
            recoveryStateView(status = RecoveryStatusView.SWEEP_CONFIRMED),
            dismissed = false,
        )!!
        assertEquals("Funds recovered!", banner.title)
        assertEquals("Available in approximately 14 days", banner.subtitle)
        assertFalse(banner.navigatesToRecover)
        assertTrue(banner.dismissible)
    }

    @Test
    fun dismissalHidesOnlyTheSweepConfirmedVariant() {
        assertNull(
            recoveryBanner(
                recoveryStateView(status = RecoveryStatusView.SWEEP_CONFIRMED),
                dismissed = true,
            ),
        )
        // Needs-recovery has no dismiss affordance; a stale flag never hides it.
        assertEquals(
            "Your funds are safe",
            recoveryBanner(
                recoveryStateView(status = RecoveryStatusView.NEEDS_RECOVERY),
                dismissed = true,
            )?.title,
        )
    }

    @Test
    fun recoveryStateChangeResetsTheDismissal() {
        val dismissed = UiState(recoveryBannerDismissed = true)
        assertFalse(reduce(dismissed, Event.RecoveryStateChanged).recoveryBannerDismissed)
    }

    // --- PendingSweepBanner ---

    @Test
    fun sweepBannerOnlyShowsAfterAFailedAttempt() {
        assertNull(sweepBanner(null))
        assertNull(sweepBanner(pendingSweepView(lastAttemptFailed = false)))
    }

    @Test
    fun headingShowsTheAmountWaitingToSweep() {
        val banner = sweepBanner(pendingSweepView(pendingSats = 5_000uL))!!
        assertEquals("₿5,000 waiting to sweep", banner.heading)
    }

    @Test
    fun unknownValueUndercountsGetAnAtLeastPrefix() {
        val banner = sweepBanner(
            pendingSweepView(pendingSats = 5_000uL, hasUnknownValue = true),
        )!!
        assertEquals("At least ₿5,000 waiting to sweep", banner.heading)
    }

    @Test
    fun zeroPendingFallsBackToGenericHeading() {
        val banner = sweepBanner(pendingSweepView(pendingSats = 0uL))!!
        assertEquals("Funds waiting to sweep", banner.heading)
    }

    @Test
    fun needsFundsWithShortfallAsksForAtLeastThatAmountAndLinksToReceive() {
        val banner = sweepBanner(
            pendingSweepView(needsOnchainFunds = true, shortfallSats = 800uL),
        )!!
        assertEquals(
            "Add at least ₿800 to cover network fees and recover these funds",
            banner.subtitle,
        )
        assertTrue(banner.navigatesToReceive)
    }

    @Test
    fun needsFundsWithoutShortfallUsesTheGenericAsk() {
        val banner = sweepBanner(
            pendingSweepView(needsOnchainFunds = true, shortfallSats = null),
        )!!
        assertEquals("Add bitcoin to cover network fees and recover these funds", banner.subtitle)
        assertTrue(banner.navigatesToReceive)
    }

    @Test
    fun selfSufficientSweepIsInformationalOnly() {
        val banner = sweepBanner(pendingSweepView(needsOnchainFunds = false))!!
        assertEquals(
            "Recovered funds return to your balance automatically when network fees allow",
            banner.subtitle,
        )
        assertFalse(banner.navigatesToReceive)
    }
}
