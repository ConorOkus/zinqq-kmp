package zinqq.app.screens.settings

import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertFalse
import kotlin.test.assertNull
import kotlin.test.assertTrue
import uniffi.wallet_core.CloseFeePayer
import uniffi.wallet_core.WalletException

/**
 * CloseChannel's pure derivations (U17, R10 UI): the coop/force confirm
 * variants, informational estimate rendering with every field independently
 * nullable (`CloseChannel.tsx:276-293`), the non-anchor and in-flight
 * warnings (`CloseChannel.tsx:399-413`), the success copy
 * (`CloseChannel.tsx:192-206`), and the force-close escalation offer
 * (`CloseChannel.tsx:139-146,239-252`).
 */
class CloseChannelLogicTest {

    // --- estimated cost (CloseChannel.tsx:276-281) ---

    @Test
    fun costLabelUsesTheVariantTotal() {
        val estimate = closeEstimate(
            coopTotalYouPaySats = 300uL,
            forceTotalYouPaySats = 2_500uL,
        )
        assertEquals("~₿300", closeCostLabel(estimate, force = false, loading = false))
        assertEquals("~₿2,500", closeCostLabel(estimate, force = true, loading = false))
    }

    @Test
    fun costLabelHandlesLoadingAndUnavailable() {
        assertEquals("Estimating…", closeCostLabel(null, force = false, loading = true))
        assertEquals(
            "Estimate unavailable",
            closeCostLabel(closeEstimate(), force = false, loading = false),
        )
        assertEquals("Estimate unavailable", closeCostLabel(null, force = true, loading = false))
    }

    // --- funds-available timeline (CloseChannel.tsx:282-286) ---

    @Test
    fun coopTimelineIsMinutes() {
        assertEquals(
            "~minutes once confirmed",
            closeTimelineLabel(closeEstimate(timelockBlocks = 144u), force = false),
        )
    }

    @Test
    fun forceTimelineHumanizesTheTimelock() {
        assertEquals(
            "up to ~24 hours",
            closeTimelineLabel(closeEstimate(timelockBlocks = 144u), force = true),
        )
        assertEquals("up to ~14 days", closeTimelineLabel(closeEstimate(), force = true))
        assertEquals("up to ~14 days", closeTimelineLabel(null, force = true))
    }

    // --- you-get-back (CloseChannel.tsx:287-291) ---

    @Test
    fun expectedBackRendersValueLoadingOrPlaceholder() {
        assertEquals(
            "~₿54,321",
            expectedBackLabel(closeEstimate(expectedBackSats = 54_321uL), loading = false),
        )
        assertEquals("Estimating…", expectedBackLabel(null, loading = true))
        assertEquals("—", expectedBackLabel(closeEstimate(), loading = false))
    }

    // --- notes and warnings ---

    @Test
    fun lspPaysNoteOnlyForCoopWithCounterpartyFunder() {
        val counterpartyFunded = closeEstimate(feePayer = CloseFeePayer.COUNTERPARTY)
        assertTrue(lspPaysCloseFee(counterpartyFunded, force = false))
        assertFalse(lspPaysCloseFee(counterpartyFunded, force = true))
        assertFalse(lspPaysCloseFee(closeEstimate(feePayer = CloseFeePayer.YOU), force = false))
        assertFalse(lspPaysCloseFee(null, force = false))
    }

    @Test
    fun nonAnchorWarningOnlyForForceWithKnownNonAnchor() {
        assertTrue(showsNonAnchorWarning(closeEstimate(isAnchor = false), force = true))
        assertFalse(showsNonAnchorWarning(closeEstimate(isAnchor = false), force = false))
        assertFalse(showsNonAnchorWarning(closeEstimate(isAnchor = true), force = true))
        assertFalse(showsNonAnchorWarning(closeEstimate(isAnchor = null), force = true))
        assertFalse(showsNonAnchorWarning(null, force = true))
    }

    @Test
    fun inFlightWarningPluralizesLikeThePwa() {
        assertNull(pendingHtlcWarning(closeEstimate(pendingHtlcCount = 0u)))
        assertNull(pendingHtlcWarning(null))
        assertEquals(
            "1 in-flight payment must settle before the close completes — " +
                "the amount returned may change.",
            pendingHtlcWarning(closeEstimate(pendingHtlcCount = 1u)),
        )
        assertEquals(
            "3 in-flight payments must settle before the close completes — " +
                "the amount returned may change.",
            pendingHtlcWarning(closeEstimate(pendingHtlcCount = 3u)),
        )
    }

    // --- CTA + success copy variants ---

    @Test
    fun ctaLabelFollowsTheCloseMethod() {
        assertEquals("Close Channel", closeCtaLabel(force = false, closing = false))
        assertEquals("Force Close Channel", closeCtaLabel(force = true, closing = false))
        assertEquals("Closing…", closeCtaLabel(force = false, closing = true))
    }

    @Test
    fun successDetailVariesByMethodAndTimelock() {
        assertEquals(
            "Your channel is closing. Funds return to your wallet once the closing " +
                "transaction confirms on-chain — keep the app open until the close completes.",
            closeSuccessDetail(force = false, estimate = null),
        )
        assertEquals(
            "Force close initiated. Your funds will be accessible in ~24 hours — they " +
                "return to your wallet automatically once the timelock expires.",
            closeSuccessDetail(force = true, estimate = closeEstimate(timelockBlocks = 144u)),
        )
        assertEquals(
            "Force close initiated. Your funds will be accessible in ~14 days — they " +
                "return to your wallet automatically once the timelock expires.",
            closeSuccessDetail(force = true, estimate = null),
        )
    }

    // --- failure mapping + escalation offer ---

    @Test
    fun coopFailureOffersForceCloseEscalation() {
        val failure = closeFailure(WalletException.ChannelCloseFailed("peer offline"), force = false)
        assertEquals("Close failed: peer offline", failure.message)
        assertTrue(failure.canForceClose)
    }

    @Test
    fun forceFailureDoesNotEscalateFurther() {
        val failure = closeFailure(RuntimeException(), force = true)
        assertEquals("Force close failed.", failure.message)
        assertFalse(failure.canForceClose)
    }

    @Test
    fun unknownCoopFailureUsesThePwaDefaultCopy() {
        val failure = closeFailure(RuntimeException(), force = false)
        assertEquals(
            "Cooperative close failed. The peer may be disconnected or the channel " +
                "has pending payments.",
            failure.message,
        )
        assertTrue(failure.canForceClose)
    }
}
