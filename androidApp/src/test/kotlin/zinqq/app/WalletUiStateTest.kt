package zinqq.app

import uniffi.wallet_core.Event
import kotlin.test.Test
import kotlin.test.assertEquals
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
            UiState(),
            Event.InvoiceReady(bolt11 = BOLT11, expiryUnixSecs = 1_753_500_000uL),
        )

        assertEquals(BOLT11, state.currentInvoice?.bolt11)
        assertEquals(1_753_500_000uL, state.currentInvoice?.expiryUnixSecs)
    }

    @Test
    fun paymentReceivedReportsOutcomeAndClearsTheInvoice() {
        val displaying = reduce(
            UiState(),
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

        assertNull(state.currentInvoice)
        assertEquals("Received 250 sats (LSP fee 12 sats)", state.lastOutcome)
    }

    @Test
    fun paymentFailedShowsTheFailureReason() {
        val state = reduce(
            UiState(),
            Event.PaymentFailed(paymentHash = PAYMENT_HASH, reason = "no route found"),
        )

        assertEquals("Payment failed: no route found", state.lastOutcome)
    }

    @Test
    fun syncFailureRaisesTheBannerAndSyncCompletionClearsIt() {
        val degraded = reduce(UiState(), Event.SyncFailed)
        assertEquals("Chain sync failed — retrying", degraded.syncBanner)

        assertNull(reduce(degraded, Event.SyncCompleted).syncBanner)
    }

    @Test
    fun fencedEventRaisesTheBlockingFencedFlag() {
        val state = reduce(
            UiState(),
            Event.Fenced(detail = "vss 409: divergent channel_manager"),
        )

        assertTrue(state.fenced)
    }

    @Test
    fun noEventClearsTheFencedFlag() {
        // Un-fencing is user-owned (KTD-3, System-Wide Impact): restore or
        // quit — a node stop must not lower the fence.
        val fenced = reduce(UiState(), Event.Fenced(detail = "409"))

        assertTrue(reduce(fenced, Event.NodeStopped).fenced)
        assertTrue(reduce(fenced, Event.SyncCompleted).fenced)
    }

    private companion object {
        // Opaque display data to the shell (R14); never parsed by Android code.
        const val BOLT11 = "lnbc2500u1pvjluezpp5qqqsyqcyq5rqwzqfqqqsyqcyq5rqwzqfqqqsyqcyq5rqwzqfqypq"
        val PAYMENT_HASH = "11".repeat(32)
    }
}
