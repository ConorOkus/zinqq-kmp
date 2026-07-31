package zinqq.app.screens.send

import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertIs
import kotlin.test.assertNull
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.cancel
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.MutableSharedFlow
import kotlinx.coroutines.test.StandardTestDispatcher
import kotlinx.coroutines.test.TestScope
import kotlinx.coroutines.test.advanceUntilIdle
import kotlinx.coroutines.test.runCurrent
import kotlinx.coroutines.test.runTest
import uniffi.wallet_core.ClassifiedKind
import uniffi.wallet_core.ClassifiedView
import uniffi.wallet_core.Event
import uniffi.wallet_core.FeeEstimate
import uniffi.wallet_core.LnurlPayView
import uniffi.wallet_core.MaxSendEstimate
import uniffi.wallet_core.ResolvedView

/**
 * The dispatch → outcome await over a fake port (U15, F1).
 *
 * The core's 5-minute cap leaves a timed-out payment in flight, so an outcome
 * event carrying another payment's hash must never settle *this* send — the
 * failure mode is telling the user a payment failed when it succeeded, or the
 * reverse. BOLT12 has no hash before the invoice request, so offers keep
 * first-outcome matching.
 */
class SendControllerTest {

    private class FakePort(private val classified: ClassifiedView) : SendPort {
        override val explorerBaseUrl: String = "https://mempool.space"
        val events = MutableSharedFlow<Event>(extraBufferCapacity = 8)
        var bolt11Sends = 0
        var offerPayments = 0
        var lastOverrideMsat: ULong? = null

        override suspend fun classify(input: String): ClassifiedView = classified

        override suspend fun sendBolt11(bolt11: String, amountMsat: ULong?) {
            bolt11Sends++
            lastOverrideMsat = amountMsat
        }

        override suspend fun payOffer(offer: String, amountMsat: ULong?) {
            offerPayments++
            lastOverrideMsat = amountMsat
        }

        override val walletEvents: Flow<Event> = events

        override fun lightningCapacityMsat(): ULong = 1_000_000uL

        override fun onchainBalanceSats(): ULong = 50_000uL

        // Unused by the dispatch path; the on-chain and resolution branches
        // have their own coverage in SendFlowTest.
        override suspend fun resolve(input: String): ResolvedView = unused()
        override suspend fun fetchLnurlInvoice(
            lnurl: LnurlPayView,
            amountMsat: ULong,
        ): ClassifiedView = unused()
        override suspend fun estimateOnchainFee(
            address: String,
            amountSats: ULong,
        ): FeeEstimate = unused()
        override suspend fun estimateMaxSendable(address: String): MaxSendEstimate = unused()
        override suspend fun sendOnchain(
            address: String,
            amountSats: ULong,
            expectedAmountSats: ULong,
            expectedFeeSats: ULong,
        ): String = unused()
        override suspend fun sendOnchainMax(
            address: String,
            expectedAmountSats: ULong,
            expectedFeeSats: ULong,
        ): String = unused()

        private fun unused(): Nothing = error("not exercised by the dispatch path")
    }

    /**
     * Drive [body] against a controller on the test scheduler. The outcome cap
     * is virtual-time, so the body uses [runCurrent] (never
     * `advanceUntilIdle`) once a dispatch is awaiting — advancing to idle would
     * fire the timeout instead of delivering the event under test.
     */
    private fun controllerTest(
        port: FakePort,
        body: TestScope.(SendController) -> Unit,
    ) = runTest {
        val scope = CoroutineScope(StandardTestDispatcher(testScheduler))
        val controller = SendController(port, scope, outcomeTimeoutMs = 60_000)
        try {
            body(controller)
        } finally {
            scope.cancel()
        }
    }

    private fun bolt11View(hash: String?) = classifiedView(
        kind = ClassifiedKind.BOLT11,
        bolt11 = TEST_BOLT11,
        paymentHash = hash,
        amountMsat = 50_000uL,
    )

    private fun TestScope.dispatch(controller: SendController, input: String): SendStep.Dispatching {
        controller.submitInput(input)
        advanceUntilIdle()
        assertIs<SendStep.ReviewLightning>(controller.step.value)
        controller.confirmLightning()
        runCurrent()
        return assertIs<SendStep.Dispatching>(controller.step.value)
    }

    @Test
    fun aForeignOutcomeHashDoesNotSettleTheDispatch() {
        val port = FakePort(bolt11View(TEST_PAYMENT_HASH))
        controllerTest(port) { controller ->
            val dispatching = dispatch(controller, TEST_BOLT11)
            assertEquals(TEST_PAYMENT_HASH, dispatching.paymentHash)
            assertEquals(1, port.bolt11Sends)
            // Embedded amount → no override (core U6 matrix).
            assertNull(port.lastOverrideMsat)

            // A send from an earlier visit outlived its 5-minute cap and only
            // now succeeds: it is not ours, so the await keeps waiting.
            port.events.tryEmit(Event.PaymentSuccessful(OTHER_PAYMENT_HASH, feePaidMsat = 3uL))
            runCurrent()
            assertIs<SendStep.Dispatching>(controller.step.value)

            // A failure for that same foreign payment is ignored too.
            port.events.tryEmit(Event.PaymentFailed(OTHER_PAYMENT_HASH, "no route"))
            runCurrent()
            assertIs<SendStep.Dispatching>(controller.step.value)
        }
    }

    @Test
    fun ourOwnHashSettlesTheDispatch() {
        val port = FakePort(bolt11View(TEST_PAYMENT_HASH))
        controllerTest(port) { controller ->
            dispatch(controller, TEST_BOLT11)

            port.events.tryEmit(Event.PaymentSuccessful(OTHER_PAYMENT_HASH, feePaidMsat = null))
            runCurrent()
            port.events.tryEmit(Event.PaymentSuccessful(TEST_PAYMENT_HASH, feePaidMsat = 1uL))
            runCurrent()
            // 50_000 msat → ₿50 (ceil).
            assertEquals(SendStep.Success(amountSats = 50uL), controller.step.value)
        }
    }

    @Test
    fun ourOwnFailureHashSettlesToTheCoresReason() {
        val port = FakePort(bolt11View(TEST_PAYMENT_HASH))
        controllerTest(port) { controller ->
            dispatch(controller, TEST_BOLT11)
            port.events.tryEmit(Event.PaymentFailed(TEST_PAYMENT_HASH, "no route"))
            runCurrent()
            assertEquals(SendStep.Failure(message = "no route"), controller.step.value)
        }
    }

    @Test
    fun bolt12DispatchStillSettlesOnTheFirstOutcome() {
        val port = FakePort(
            classifiedView(
                kind = ClassifiedKind.BOLT12,
                offer = TEST_OFFER,
                amountMsat = 50_000uL,
            ),
        )
        controllerTest(port) { controller ->
            val dispatching = dispatch(controller, TEST_OFFER)
            // No hash exists before the invoice request, so nothing to filter on.
            assertNull(dispatching.paymentHash)
            assertEquals(1, port.offerPayments)

            port.events.tryEmit(Event.PaymentSuccessful(OTHER_PAYMENT_HASH, feePaidMsat = null))
            runCurrent()
            assertEquals(SendStep.Success(amountSats = 50uL), controller.step.value)
        }
    }
}
