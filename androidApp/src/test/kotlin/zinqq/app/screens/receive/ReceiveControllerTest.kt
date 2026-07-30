package zinqq.app.screens.receive

import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertFalse
import kotlin.test.assertIs
import kotlin.test.assertNull
import kotlin.test.assertTrue
import kotlinx.coroutines.CompletableDeferred
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
import uniffi.wallet_core.AsyncReceiveStatus
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

        /** The offer the core mints; `null` stands in for "every attempt failed". */
        var mintedOffer: String? = TEST_RECEIVE_OFFER
        var mintFails = false
        var mintCalls = 0

        /** Held open, this stands in for the core's retry schedule still running. */
        var mintGate: CompletableDeferred<Unit>? = null

        /** Async receive: DISABLED is the shipped default. */
        var asyncStatus: AsyncReceiveStatus = AsyncReceiveStatus.DISABLED
        var asyncOffer: String? = TEST_ASYNC_RECEIVE_OFFER
        var asyncFails = false

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

        override suspend fun getOrCreateOffer(): String? {
            mintCalls++
            mintGate?.await()
            if (mintFails) throw WalletException.NotRunning()
            return mintedOffer
        }

        override suspend fun bolt12Uri(offer: String): String = "bitcoin:?lno=$offer"

        override suspend fun asyncReceiveOffer(): String? {
            if (asyncFails) throw WalletException.NotRunning()
            return asyncOffer
        }

        override suspend fun asyncReceiveStatus(): AsyncReceiveStatus {
            if (asyncFails) throw WalletException.NotRunning()
            return asyncStatus
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
    fun capacityCoveredEntryMintsTheMissingOfferAndRendersItsPage() {
        val port = FakePort().apply {
            inboundMsat = 100_000_000uL
            // A usable channel (amountless needsJit = false) but nothing
            // persisted yet — the core mints on demand.
            bundleFor = { makeBundle(offer = null) }
        }
        controllerTest(port) { c ->
            advanceUntilIdle()
            val state = c.state.value
            assertEquals(1, port.mintCalls)
            assertEquals(TEST_RECEIVE_OFFER, state.offer)
            assertEquals("bitcoin:?lno=$TEST_RECEIVE_OFFER".uppercase(), state.offerQrValue)
            assertTrue(showBolt12Page(state.offerQrValue != null, state.needsAmount))
        }
    }

    @Test
    fun entryWithAPersistedOfferNeverMintsAgain() {
        val port = FakePort().apply {
            inboundMsat = 100_000_000uL
            bundleFor = { makeBundle(offer = TEST_RECEIVE_OFFER) }
        }
        controllerTest(port) {
            advanceUntilIdle()
            assertEquals(0, port.mintCalls)
        }
    }

    @Test
    fun freshWalletEntryNeverMintsAnOffer() {
        // No usable channel: the page could not render, so the ~93 s
        // creation retry schedule must not run at all.
        val port = freshWalletPort()
        controllerTest(port) {
            advanceUntilIdle()
            assertEquals(0, port.mintCalls)
        }
    }

    @Test
    fun failedOfferCreationLeavesReceiveIntact() {
        val port = FakePort().apply {
            inboundMsat = 100_000_000uL
            bundleFor = { makeBundle(offer = null) }
            mintFails = true
        }
        controllerTest(port) { c ->
            advanceUntilIdle()
            val state = c.state.value
            assertNull(state.offer)
            assertNull(state.offerQrValue)
            assertNull(state.loadError)
            assertEquals(ReceiveStep.Display(InvoicePath.STANDARD), state.step)
            assertFalse(showBolt12Page(state.offerQrValue != null, state.needsAmount))
        }
    }

    @Test
    fun aLateOfferLandsBesideTheJitFlowWithoutClobberingIt() {
        val gate = CompletableDeferred<Unit>()
        val port = FakePort().apply {
            inboundMsat = 100_000_000uL
            mintGate = gate
            bundleFor = { amountMsat ->
                // Amountless: capacity covered, so the mint gate opens.
                // Amounted: over capacity, so the visit runs the JIT flow
                // while creation is still retrying.
                makeBundle(offer = null, needsJit = amountMsat != null)
            }
        }
        controllerTest(port) { c ->
            c.enterAmount("200000")
            advanceUntilIdle()
            assertIs<ReceiveStep.JitReview>(c.state.value.step)

            // Creation finally succeeds mid-review.
            gate.complete(Unit)
            advanceUntilIdle()

            val state = c.state.value
            assertEquals(TEST_RECEIVE_OFFER, state.offer)
            assertEquals("bitcoin:?lno=$TEST_RECEIVE_OFFER".uppercase(), state.offerQrValue)
            assertIs<ReceiveStep.JitReview>(state.step)
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

    // --- async payments receive (U5) ---

    /** A visit with a usable channel: the standard offer page is eligible. */
    private fun asyncPort() = FakePort().apply {
        bundleFor = { makeBundle(offer = TEST_RECEIVE_OFFER) }
        inboundMsat = 500_000_000uL
    }

    /**
     * READY plus an offer is the only state that adds the page — and it adds
     * it BESIDE the standard offer page, never instead of it.
     */
    @Test
    fun readyAsyncOfferAddsAPageBesideTheStandardOffer() {
        val port = asyncPort().apply { asyncStatus = AsyncReceiveStatus.READY }
        controllerTest(port) { c ->
            val state = c.state.value
            assertEquals(TEST_ASYNC_RECEIVE_OFFER, state.asyncOffer)
            assertEquals(
                "bitcoin:?lno=$TEST_ASYNC_RECEIVE_OFFER".uppercase(),
                state.asyncOfferQrValue,
            )
            assertEquals(TEST_RECEIVE_OFFER, state.offer, "the standard offer survives")
            assertEquals(
                listOf(QrPage.UNIFIED, QrPage.BOLT12, QrPage.ASYNC),
                pagesFor(state),
            )
        }
    }

    /** The shipped default: nothing changes anywhere. */
    @Test
    fun disabledAsyncReceiveLeavesTheScreenUnchanged() {
        controllerTest(asyncPort()) { c ->
            val state = c.state.value
            assertNull(state.asyncOffer)
            assertNull(state.asyncOfferQrValue)
            assertEquals(listOf(QrPage.UNIFIED, QrPage.BOLT12), pagesFor(state))
        }
    }

    /** Configured but still handshaking: no page, and no empty placeholder. */
    @Test
    fun awaitingServerAddsNoPage() {
        val port = asyncPort().apply { asyncStatus = AsyncReceiveStatus.AWAITING_SERVER }
        controllerTest(port) { c ->
            val state = c.state.value
            assertNull(state.asyncOffer)
            assertEquals(listOf(QrPage.UNIFIED, QrPage.BOLT12), pagesFor(state))
        }
    }

    /**
     * Status and offer are two calls, so READY can race ahead of an offer
     * that has gone away. No page, no crash.
     */
    @Test
    fun readyWithoutAnOfferAddsNoPage() {
        val port = asyncPort().apply {
            asyncStatus = AsyncReceiveStatus.READY
            asyncOffer = null
        }
        controllerTest(port) { c ->
            val state = c.state.value
            assertNull(state.asyncOffer)
            assertEquals(listOf(QrPage.UNIFIED, QrPage.BOLT12), pagesFor(state))
        }
    }

    /** Async receive NEVER degrades receive — the core's standing contract. */
    @Test
    fun aThrowingAsyncPortLeavesTheRestOfReceiveIntact() {
        val port = asyncPort().apply {
            asyncStatus = AsyncReceiveStatus.READY
            asyncFails = true
        }
        controllerTest(port) { c ->
            val state = c.state.value
            assertNull(state.loadError)
            assertEquals(TEST_RECEIVE_ADDRESS, state.address)
            assertEquals(TEST_RECEIVE_OFFER, state.offer)
            assertNull(state.asyncOffer)
            assertEquals(listOf(QrPage.UNIFIED, QrPage.BOLT12), pagesFor(state))
        }
    }

    /** The no-channel mandatory-amount visit shows no reusable page at all. */
    @Test
    fun aFreshWalletNeverShowsTheAsyncPage() {
        val port = freshWalletPort().apply {
            asyncStatus = AsyncReceiveStatus.READY
        }
        controllerTest(port) { c ->
            val state = c.state.value
            assertNull(state.asyncOffer)
            assertEquals(listOf(QrPage.UNIFIED), pagesFor(state))
        }
    }

    private fun pagesFor(state: ReceiveUiState) = receivePages(
        offerExists = state.offerQrValue != null,
        asyncOfferExists = state.asyncOfferQrValue != null,
        needsAmount = state.needsAmount,
    )
}
