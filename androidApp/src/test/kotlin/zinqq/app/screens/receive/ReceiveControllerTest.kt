package zinqq.app.screens.receive

import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertFalse
import kotlin.test.assertIs
import kotlin.test.assertNull
import kotlin.test.assertTrue
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.cancel
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.MutableSharedFlow
import kotlinx.coroutines.test.StandardTestDispatcher
import kotlinx.coroutines.test.TestScope
import kotlinx.coroutines.test.advanceTimeBy
import kotlinx.coroutines.test.advanceUntilIdle
import kotlinx.coroutines.test.runCurrent
import kotlinx.coroutines.test.runTest
import uniffi.wallet_core.Event
import uniffi.wallet_core.JitInvoice
import uniffi.wallet_core.JitQuote
import uniffi.wallet_core.ReceiveBundle
import uniffi.wallet_core.WalletException
import zinqq.main.NumpadKey

/**
 * The controller's async transitions over a fake port (U16): mandatory
 * amount entry, quote → review → buy → JIT QR, staleness re-quote,
 * below-minimum review, the expiry flip, and the settlement watcher.
 */
class ReceiveControllerTest {

    private class FakePort : ReceivePort {
        var bundleFor: (ULong?) -> ReceiveBundle = { makeBundle() }
        var quoteFor: (ULong) -> JitQuote = { makeQuote(amountMsat = it) }
        var acceptFor: (ULong, ULong) -> JitInvoice = { _, _ -> makeJitInvoice() }
        var floorSats: ULong = 3_000uL
        var inboundMsat: ULong = 0uL
        var floorFetches = 0

        val events = MutableSharedFlow<Event>(extraBufferCapacity = 8)

        override suspend fun receiveBundle(amountMsat: ULong?): ReceiveBundle =
            bundleFor(amountMsat)

        override suspend fun jitQuote(amountMsat: ULong): JitQuote = quoteFor(amountMsat)

        override suspend fun jitAccept(quoteToken: ULong, amountMsat: ULong): JitInvoice =
            acceptFor(quoteToken, amountMsat)

        override suspend fun minReceiveSats(refresh: Boolean): ULong {
            floorFetches++
            return floorSats
        }

        override suspend fun usableInboundMsat(): ULong = inboundMsat

        override suspend fun buildUnifiedUri(
            address: String,
            amountSats: ULong?,
            invoice: String?,
        ): String = buildString {
            append("bitcoin:").append(address.uppercase())
            if (invoice != null) append("?lightning=").append(invoice)
        }

        override val walletEvents: Flow<Event> = events
    }

    private fun freshWalletPort() = FakePort().apply {
        // No channels: amountless bundle needs JIT, no bolt11, no offer.
        bundleFor = {
            makeBundle(
                bolt11 = null,
                paymentHash = null,
                needsJit = true,
                bip321Uri = "bitcoin:${TEST_RECEIVE_ADDRESS.uppercase()}",
                offer = null,
                offerQrValue = null,
            )
        }
    }

    /**
     * Run [body] against a started controller. The controller gets its own
     * scope on the test scheduler — `backgroundScope` work does not advance
     * from the test body in coroutines-test 1.10 — cancelled afterwards so
     * the settlement watcher's endless collect cannot leak across tests.
     */
    private fun controllerTest(
        port: FakePort,
        body: TestScope.(ReceiveController) -> Unit,
    ) = runTest {
        val scope = CoroutineScope(StandardTestDispatcher(testScheduler))
        val controller = ReceiveController(port, scope) { 0L }
        controller.start()
        advanceUntilIdle()
        try {
            body(controller)
        } finally {
            scope.cancel()
        }
    }

    private fun ReceiveController.enterAmount(digits: String) {
        digits.forEach { onNumpadKey(NumpadKey.Digit(it)) }
        confirmAmount()
    }

    @Test
    fun freshWalletEntryOpensTheMandatoryNumpad() = controllerTest(freshWalletPort()) { c ->
        val state = c.state.value
        assertFalse(state.loading)
        assertTrue(state.needsAmount)
        assertTrue(state.editingAmount)
        assertEquals("Request", numpadCtaLabel(state.needsAmount, state.confirmedAmountSats))
    }

    @Test
    fun freshWalletEntryFetchesTheLiveFloorOnce() {
        val port = freshWalletPort()
        controllerTest(port) {
            // R6: the one live-floor fetch per visit fired (no capacity).
            assertEquals(1, port.floorFetches)
        }
    }

    @Test
    fun capacityCoveredEntrySkipsTheFloorFetchAndShowsTheStandardQr() {
        val port = FakePort().apply {
            inboundMsat = 100_000_000uL // covers the static floor
            bundleFor = { makeBundle(offer = TEST_RECEIVE_OFFER) }
        }
        controllerTest(port) { c ->
            val state = c.state.value
            assertFalse(state.needsAmount)
            assertFalse(state.editingAmount)
            assertEquals(ReceiveStep.Display(InvoicePath.STANDARD), state.step)
            assertEquals(0, port.floorFetches)
            assertTrue(showBolt12Page(state.offerQrValue != null, state.needsAmount))
        }
    }

    @Test
    fun jitConfirmRunsQuoteIntoReview() {
        val port = freshWalletPort()
        port.quoteFor = { makeQuote(amountMsat = it, openingFeeMsat = 2_500_000uL) }
        controllerTest(port) { c ->
            c.enterAmount("10000")
            // The quoting skeleton presents in the same update as the commit.
            assertEquals(ReceiveStep.Quoting, c.state.value.step)
            advanceUntilIdle()

            val review = assertIs<ReceiveStep.JitReview>(c.state.value.step)
            assertEquals(10_000uL, review.amountSats)
            assertEquals(2_500uL, review.setupFeeSats)
            assertEquals(7_500uL, review.youReceiveSats)
            assertFalse(review.quoteUpdated)
        }
    }

    @Test
    fun belowFloorConfirmIsBlockedBeforeAnyQuote() {
        // AE4: no quote (and certainly no buy) is issued below the floor.
        var quoted = false
        val port = freshWalletPort()
        port.quoteFor = { quoted = true; makeQuote(amountMsat = it) }
        controllerTest(port) { c ->
            c.enterAmount("500")
            advanceUntilIdle()

            assertTrue(c.state.value.editingAmount)
            assertEquals("", c.state.value.confirmedDigits)
            assertFalse(quoted)
        }
    }

    @Test
    fun buySuccessRendersTheJitQrWithFeeAndExpiryThenFlips() {
        val port = freshWalletPort()
        port.acceptFor = { _, _ ->
            makeJitInvoice(openingFeeMsat = 2_500_000uL, expiresAtUnix = 600uL)
        }
        controllerTest(port) { c ->
            c.enterAmount("10000")
            advanceUntilIdle()
            c.generateInvoice()
            // runCurrent, not advanceUntilIdle: the expiry timer is now
            // scheduled and advancing to idle would fast-forward past it.
            runCurrent()

            val state = c.state.value
            assertEquals(ReceiveStep.Display(InvoicePath.JIT), state.step)
            assertEquals(2_500uL, state.openingFeeSats)
            assertEquals(600uL, state.expiresAtUnix)
            assertTrue(state.qrValue.contains("LIGHTNING="))
            assertEquals(
                "Setup fee: ₿2,500",
                qrCaption(QrPage.UNIFIED, InvoicePath.JIT, state.openingFeeSats),
            )

            // The expiry flip fires when the clamped validity passes.
            advanceTimeBy(601_000)
            assertEquals(ReceiveStep.JitExpired, c.state.value.step)
        }
    }

    @Test
    fun staleBuyReQuotesTheSameLsp() {
        val port = freshWalletPort()
        var buys = 0
        port.acceptFor = { _, _ ->
            buys++
            throw WalletException.JitReQuoteRequired()
        }
        controllerTest(port) { c ->
            c.enterAmount("10000")
            advanceUntilIdle()
            c.generateInvoice()
            advanceUntilIdle()

            // Back on Review with a fresh quote, flagged as updated.
            val review = assertIs<ReceiveStep.JitReview>(c.state.value.step)
            assertTrue(review.quoteUpdated)
            assertEquals(1, buys)
        }
    }

    @Test
    fun otherBuyFailuresLandOnTheErrorScreen() {
        val port = freshWalletPort()
        port.acceptFor = { _, _ -> throw WalletException.Lsps2("lsps2.buy failed: boom") }
        controllerTest(port) { c ->
            c.enterAmount("10000")
            advanceUntilIdle()
            c.generateInvoice()
            advanceUntilIdle()

            assertEquals(ReceiveStep.JitError, c.state.value.step)
        }
    }

    @Test
    fun belowMinimumQuoteFailureShowsTheDisabledReviewAndRaisesTheGate() {
        val port = freshWalletPort()
        port.quoteFor = {
            throw WalletException.Lsps2(
                "LSPS2 request failed: amount 4000000msat is below the LSP minimum " +
                    "payment size of 5000000msat",
            )
        }
        port.floorSats = 5_500uL // the refreshed headroom-adjusted floor
        controllerTest(port) { c ->
            c.enterAmount("4000")
            advanceUntilIdle()

            val state = c.state.value
            assertEquals(
                ReceiveStep.JitBelowMinimum(4_000uL, displayMinSats = 5_500uL),
                state.step,
            )
            // The numpad gate now blocks the same amount up front.
            assertEquals(5_500uL, state.floorSats)
        }
    }

    @Test
    fun nonSizeQuoteFailureFallsBackToTheOnchainQr() {
        val port = freshWalletPort()
        port.quoteFor = { throw WalletException.Lsps2("lsps2.get_info failed: boom") }
        controllerTest(port) { c ->
            c.enterAmount("10000")
            advanceUntilIdle()

            assertEquals(ReceiveStep.Display(InvoicePath.NONE), c.state.value.step)
        }
    }

    @Test
    fun matchingPaymentSettlesTheVisit() {
        val port = FakePort().apply {
            inboundMsat = 100_000_000uL
            bundleFor = { makeBundle(paymentHash = "feed") }
        }
        controllerTest(port) { c ->
            port.events.tryEmit(
                Event.PaymentReceived(
                    paymentHash = "feed",
                    amountMsat = 10_000_000uL,
                    skimmedFeeMsat = null,
                ),
            )
            advanceUntilIdle()

            assertEquals(ReceiveStep.Received(10_000uL), c.state.value.step)
        }
    }

    @Test
    fun mismatchedPaymentDoesNotSettle() {
        val port = FakePort().apply {
            inboundMsat = 100_000_000uL
            bundleFor = { makeBundle(paymentHash = "feed") }
        }
        controllerTest(port) { c ->
            port.events.tryEmit(
                Event.PaymentReceived(
                    paymentHash = "beef",
                    amountMsat = 10_000_000uL,
                    skimmedFeeMsat = null,
                ),
            )
            advanceUntilIdle()

            assertEquals(ReceiveStep.Display(InvoicePath.STANDARD), c.state.value.step)
        }
    }

    @Test
    fun backFromReviewRestoresTheNumpadWithTheAmountPreserved() {
        val port = freshWalletPort()
        controllerTest(port) { c ->
            c.enterAmount("10000")
            advanceUntilIdle()
            c.backFromReview()
            advanceUntilIdle()

            val state = c.state.value
            assertTrue(state.editingAmount)
            assertEquals("10000", state.amountDigits)
            assertEquals("", state.confirmedDigits)
        }
    }

    @Test
    fun removeAmountStaysOnTheNumpadWhenAmountIsMandatory() {
        val port = freshWalletPort()
        controllerTest(port) { c ->
            c.enterAmount("10000")
            advanceUntilIdle()
            c.removeAmount()
            advanceUntilIdle()

            val state = c.state.value
            assertTrue(state.editingAmount)
            assertEquals("", state.amountDigits)
            assertEquals("", state.confirmedDigits)
        }
    }

    @Test
    fun entryFailureShowsTheLoadError() {
        val port = FakePort().apply {
            bundleFor = { throw WalletException.NotRunning() }
        }
        controllerTest(port) { c ->
            val state = c.state.value
            assertFalse(state.loading)
            assertNull(state.address)
            assertTrue(state.loadError != null)
        }
    }
}
