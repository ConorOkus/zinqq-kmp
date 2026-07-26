package zinqq.spike.android

import uniffi.wallet_core.Event
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertFalse
import kotlin.test.assertNull
import kotlin.test.assertTrue

/**
 * Pure [reduce] tests: no Android framework, no wallet instance. They compile
 * against the `uniffi.wallet_core` bindings Gobley generates at build time.
 */
class WalletUiStateTest {
    @Test
    fun invoiceReadyExposesQrPayloadAndExpiry() {
        val state = reduce(
            UiState(nodeRunning = true),
            Event.InvoiceReady(bolt11 = BOLT11, expiryUnixSecs = 1_753_500_000uL),
        )

        assertEquals(BOLT11, state.currentInvoice?.bolt11)
        assertEquals(1_753_500_000uL, state.currentInvoice?.expiryUnixSecs)
    }

    @Test
    fun paymentReceivedBumpsBalanceReportsOutcomeAndClearsTheInvoice() {
        val displaying = reduce(
            UiState(nodeRunning = true, balanceMsat = 5_000uL),
            Event.InvoiceReady(bolt11 = BOLT11, expiryUnixSecs = 1_753_500_000uL),
        )

        val state = reduce(
            displaying,
            Event.PaymentReceived(
                paymentHash = PAYMENT_HASH,
                amountMsat = 250_000uL,
                skimmedFeeMsat = 12_000uL,
            ),
        )

        assertEquals(255_000uL, state.balanceMsat)
        assertNull(state.currentInvoice)
        assertEquals("Received 250 sats (LSP fee 12 sats)", state.lastOutcome)
    }

    @Test
    fun paymentFailedShowsTheFailureReason() {
        val state = reduce(
            UiState(nodeRunning = true),
            Event.PaymentFailed(paymentHash = PAYMENT_HASH, reason = "no route found"),
        )

        assertEquals("Payment failed: no route found", state.lastOutcome)
    }

    @Test
    fun syncFailureRaisesTheBannerAndSyncCompletionClearsIt() {
        val degraded = reduce(UiState(nodeRunning = true), Event.SyncFailed)
        assertEquals("Chain sync failed — retrying", degraded.syncBanner)

        assertNull(reduce(degraded, Event.SyncCompleted).syncBanner)
    }

    @Test
    fun nodeLifecycleTogglesRunningState() {
        val started = reduce(UiState(), Event.NodeStarted)
        assertTrue(started.nodeRunning)
        assertFalse(reduce(started, Event.NodeStopped).nodeRunning)
    }

    private companion object {
        // Opaque display data to the shell (R4); never parsed by Android code.
        const val BOLT11 = "lnbc2500u1pvjluezpp5qqqsyqcyq5rqwzqfqqqsyqcyq5rqwzqfqqqsyqcyq5rqwzqfqypq"
        val PAYMENT_HASH = "11".repeat(32)
    }
}
