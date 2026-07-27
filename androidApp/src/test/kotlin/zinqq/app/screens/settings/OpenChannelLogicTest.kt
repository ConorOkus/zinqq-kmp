package zinqq.app.screens.settings

import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertNull
import uniffi.wallet_core.OpenFeeEstimate
import uniffi.wallet_core.WalletException

/**
 * OpenChannel's pure derivations (U17, R10 UI): the PWA's 20,000–16,777,215
 * bounds and balance gate (`OpenChannel.tsx:29-31,83-111`), the review fee /
 * total math (`OpenChannel.tsx:97-98,262-273`), and the typed-error copy.
 */
class OpenChannelLogicTest {

    private val fee = OpenFeeEstimate(feeRateSatPerVb = 3uL, estimatedFeeSats = 420uL)

    // --- bounds matrix (PWA copy verbatim) ---

    @Test
    fun belowMinimumIsRejected() {
        assertEquals(
            "Minimum channel size is ₿20,000",
            validateOpenAmount(19_999uL, fee.estimatedFeeSats, balanceSats = 1_000_000uL),
        )
    }

    @Test
    fun boundsAreInclusive() {
        assertNull(validateOpenAmount(20_000uL, fee.estimatedFeeSats, 1_000_000uL))
        assertNull(validateOpenAmount(16_777_215uL, 0uL, 20_000_000uL))
    }

    @Test
    fun aboveMaximumIsRejected() {
        assertEquals(
            "Maximum channel size is ₿16,777,215",
            validateOpenAmount(16_777_216uL, 0uL, 20_000_000uL),
        )
    }

    @Test
    fun amountPlusFeeMustFitTheBalance() {
        // 20,000 + 420 > 20,419 → rejected; = 20,420 → allowed.
        assertEquals(
            "Amount plus fees exceeds available balance",
            validateOpenAmount(20_000uL, 420uL, 20_419uL),
        )
        assertNull(validateOpenAmount(20_000uL, 420uL, 20_420uL))
    }

    // --- review derivations ---

    @Test
    fun feeRowLabelsTheRateAndTotalAddsTheFee() {
        assertEquals("Est. fee (~3 sat/vB)", openFeeRateLabel(fee.feeRateSatPerVb))
        assertEquals(20_420uL, openTotalSats(20_000uL, fee.estimatedFeeSats))
    }

    @Test
    fun estimateFallbackMirrorsThePwaOneSatPerVb() {
        // PWA `getFeeRate` failure → 1 sat/vB × 140 vB (`OpenChannel.tsx:70-72,97-98`).
        val fallback = fallbackOpenFee()
        assertEquals(1uL, fallback.feeRateSatPerVb)
        assertEquals(140uL, fallback.estimatedFeeSats)
    }

    @Test
    fun reviewPeerIsMidTruncated() {
        assertEquals(
            PEER_PUBKEY.take(12) + "..." + PEER_PUBKEY.takeLast(8),
            reviewPeerDisplay(PEER_PUBKEY),
        )
    }

    // --- typed-error copy ---

    @Test
    fun typedOpenErrorsCarryTheCoreParityCopy() {
        assertEquals(
            "Minimum channel size is ₿20,000",
            openChannelErrorMessage(WalletException.ChannelAmountBelowMinimum()),
        )
        assertEquals(
            "Maximum channel size is ₿16,777,215",
            openChannelErrorMessage(WalletException.ChannelAmountAboveMaximum()),
        )
        assertEquals(
            "Amount plus fees exceeds available balance",
            openChannelErrorMessage(WalletException.ChannelAmountExceedsBalance()),
        )
        assertEquals(
            "Failed to connect to peer: dial timed out",
            openChannelErrorMessage(WalletException.PeerConnectFailed("dial timed out")),
        )
        assertEquals(
            "Failed to initiate channel opening: rejected",
            openChannelErrorMessage(WalletException.ChannelOpenFailed("rejected")),
        )
        assertEquals(
            "Invalid peer address: expected pubkey@host:port",
            openChannelErrorMessage(
                WalletException.InvalidPeerAddress("Invalid peer address: expected pubkey@host:port"),
            ),
        )
    }

    @Test
    fun unknownOpenFailuresFallBackToThePwaGenericCopy() {
        assertEquals(
            "Failed to initiate channel opening. The peer may have disconnected.",
            openChannelErrorMessage(RuntimeException()),
        )
    }
}
