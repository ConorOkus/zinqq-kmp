package zinqq.app.screens.send

import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertIs
import kotlin.test.assertNull
import kotlin.test.assertTrue
import uniffi.wallet_core.ClassifiedKind
import uniffi.wallet_core.Event
import uniffi.wallet_core.WalletException
import zinqq.main.NumpadKey

/**
 * The send machine's step-transition matrix (U15): every classification kind
 * routes exactly like the PWA's `Send.tsx`, the gates carry its copy, and
 * the drift/timeout re-renders are pure data transforms.
 */
class SendFlowTest {

    private val capacityMsat = 1_000_000uL // ₿1,000 outbound
    private val onchainSats = 50_000uL

    private fun route(
        raw: String,
        view: uniffi.wallet_core.ClassifiedView,
        lnurl: uniffi.wallet_core.LnurlPayView? = null,
    ) = routeInput(raw, view, lnurl, capacityMsat, onchainSats)

    // --- input → routing per kind ---

    @Test
    fun invalidInputShowsTheCoreErrorVerbatim() {
        val decision = route(
            "junk",
            classifiedView(kind = ClassifiedKind.INVALID, error = "Unrecognized payment format"),
        )
        assertEquals(SendDecision.InlineError("Unrecognized payment format"), decision)
    }

    @Test
    fun bip353NameRequiresResolution() {
        val decision = route(
            "satoshi@zinqq.app",
            classifiedView(
                kind = ClassifiedKind.BIP353,
                bip353User = "satoshi",
                bip353Domain = "zinqq.app",
            ),
        )
        assertEquals(SendDecision.Resolve("satoshi@zinqq.app"), decision)
    }

    @Test
    fun fixedAmountBolt11GoesStraightToReview() {
        val view = classifiedView(
            kind = ClassifiedKind.BOLT11,
            bolt11 = TEST_BOLT11,
            amountMsat = 50_000uL,
            description = "coffee",
        )
        val decision = route(TEST_BOLT11, view)
        val step = assertIs<SendDecision.Step>(decision).step
        val review = assertIs<SendStep.ReviewLightning>(step)
        assertEquals(50_000uL, review.amountMsat)
        assertEquals("coffee", review.recipient)
        assertNull(review.returnTo)
    }

    @Test
    fun fixedAmountBolt11OverCapacityGates() {
        val view = classifiedView(
            kind = ClassifiedKind.BOLT11,
            bolt11 = TEST_BOLT11,
            amountMsat = capacityMsat + 1uL,
        )
        assertEquals(SendDecision.InlineError("Not enough funds"), route(TEST_BOLT11, view))
    }

    @Test
    fun amountlessBolt11EntersAmountStep() {
        val view = classifiedView(kind = ClassifiedKind.BOLT11, bolt11 = TEST_BOLT11)
        val step = assertIs<SendDecision.Step>(route(TEST_BOLT11, view)).step
        val amount = assertIs<SendStep.Amount>(step)
        assertEquals(TEST_BOLT11, amount.rawInput)
        assertNull(amount.minSats)
    }

    @Test
    fun fixedAmountBolt12GoesStraightToReview() {
        val view = classifiedView(
            kind = ClassifiedKind.BOLT12,
            offer = TEST_OFFER,
            amountMsat = 21_000uL,
        )
        val step = assertIs<SendDecision.Step>(route(TEST_OFFER, view)).step
        assertEquals(21_000uL, assertIs<SendStep.ReviewLightning>(step).amountMsat)
    }

    @Test
    fun amountlessBolt12EntersAmountStep() {
        val view = classifiedView(kind = ClassifiedKind.BOLT12, offer = TEST_OFFER)
        assertIs<SendStep.Amount>(assertIs<SendDecision.Step>(route(TEST_OFFER, view)).step)
    }

    @Test
    fun lnurlWithRangeEntersAmountStepWithBounds() {
        val lnurl = lnurlPayView(minSats = 10uL, maxSats = 5_000uL)
        val view = classifiedView(kind = ClassifiedKind.LNURL, description = lnurl.description)
        val step = assertIs<SendDecision.Step>(route("satoshi@zinqq.app", view, lnurl)).step
        val amount = assertIs<SendStep.Amount>(step)
        assertEquals(10uL, amount.minSats)
        assertEquals(5_000uL, amount.maxSats)
    }

    @Test
    fun lnurlMinEqualsMaxSkipsAmountEntry() {
        val lnurl = lnurlPayView(
            minSendableMsat = 5_000_000uL,
            maxSendableMsat = 5_000_000uL,
            minSats = 5_000uL,
            maxSats = 5_000uL,
            skipAmountEntry = true,
        )
        val view = classifiedView(kind = ClassifiedKind.LNURL)
        val decision = route("satoshi@zinqq.app", view, lnurl)
        val fetch = assertIs<SendDecision.FetchLnurlInvoice>(decision)
        assertEquals(5_000_000uL, fetch.amountMsat)
        assertNull(fetch.returnTo)
    }

    @Test
    fun onchainWithoutAmountEntersAmountStep() {
        val view = classifiedView(kind = ClassifiedKind.ONCHAIN, address = TEST_ADDRESS)
        assertIs<SendStep.Amount>(assertIs<SendDecision.Step>(route(TEST_ADDRESS, view)).step)
    }

    @Test
    fun onchainEmbeddedAmountRequestsEstimate() {
        val view = classifiedView(
            kind = ClassifiedKind.ONCHAIN,
            address = TEST_ADDRESS,
            amountSats = 1_000uL,
        )
        val decision = route("bitcoin:$TEST_ADDRESS?amount=0.00001", view)
        assertEquals(
            SendDecision.EstimateOnchain(TEST_ADDRESS, 1_000uL, returnTo = null),
            decision,
        )
    }

    @Test
    fun onchainEmbeddedAmountBelowDustGates() {
        val view = classifiedView(
            kind = ClassifiedKind.ONCHAIN,
            address = TEST_ADDRESS,
            amountSats = 293uL,
        )
        assertEquals(
            SendDecision.InlineError("Amount must be at least ₿294 (dust limit)"),
            route(TEST_ADDRESS, view),
        )
    }

    @Test
    fun onchainEmbeddedAmountOverBalanceGates() {
        val view = classifiedView(
            kind = ClassifiedKind.ONCHAIN,
            address = TEST_ADDRESS,
            amountSats = onchainSats + 1uL,
        )
        assertEquals(
            SendDecision.InlineError("Amount exceeds available on-chain balance"),
            route(TEST_ADDRESS, view),
        )
    }

    // --- recipient labels (PWA Send.tsx:130-136, 469-471, 592-594) ---

    @Test
    fun bip321WrappedInvoiceShowsTruncatedInvoiceLabel() {
        val view = classifiedView(
            kind = ClassifiedKind.BOLT11,
            bolt11 = TEST_BOLT11,
            amountMsat = 1_000uL,
            description = "ignored",
        )
        val step = assertIs<SendDecision.Step>(route("bitcoin:$TEST_ADDRESS?lightning=x", view)).step
        assertEquals(
            TEST_BOLT11.take(10) + "…",
            assertIs<SendStep.ReviewLightning>(step).recipient,
        )
    }

    @Test
    fun resolvedNameShowsTheNameAsRecipient() {
        val view = classifiedView(
            kind = ClassifiedKind.BOLT12,
            offer = TEST_OFFER,
            amountMsat = 1_000uL,
        )
        val step = assertIs<SendDecision.Step>(route("satoshi@zinqq.app", view)).step
        assertEquals("satoshi@zinqq.app", assertIs<SendStep.ReviewLightning>(step).recipient)
    }

    @Test
    fun plainInvoiceWithoutDescriptionShowsTruncation() {
        val view = classifiedView(
            kind = ClassifiedKind.BOLT11,
            bolt11 = TEST_BOLT11,
            amountMsat = 1_000uL,
        )
        val step = assertIs<SendDecision.Step>(route(TEST_BOLT11, view)).step
        assertEquals(
            truncateInvoice(TEST_BOLT11),
            assertIs<SendStep.ReviewLightning>(step).recipient,
        )
    }

    // --- amount step: numpad + gates ---

    @Test
    fun numpadKeyResetsErrorAndSendMax() {
        val step = SendStep.Amount(
            target = classifiedView(kind = ClassifiedKind.ONCHAIN, address = TEST_ADDRESS),
            rawInput = TEST_ADDRESS,
            digits = "10",
            isSendMax = true,
            error = "old",
        )
        val next = reduceAmountKey(step, NumpadKey.Digit('5'))
        assertEquals("105", next.digits)
        assertTrue(!next.isSendMax)
        assertNull(next.error)
    }

    @Test
    fun lnurlBelowMinimumGatesWithPwaCopy() {
        val step = SendStep.Amount(
            target = classifiedView(kind = ClassifiedKind.LNURL),
            rawInput = "satoshi@zinqq.app",
            lnurl = lnurlPayView(minSats = 1_000uL, maxSats = 10_000uL),
            digits = "999",
        )
        assertEquals(
            SendDecision.InlineError("Minimum amount is ₿1,000"),
            submitAmount(step, capacityMsat, onchainSats),
        )
    }

    @Test
    fun lnurlAboveMaximumGatesWithPwaCopy() {
        val step = SendStep.Amount(
            target = classifiedView(kind = ClassifiedKind.LNURL),
            rawInput = "satoshi@zinqq.app",
            lnurl = lnurlPayView(minSats = 1uL, maxSats = 10_000uL),
            digits = "10001",
        )
        assertEquals(
            SendDecision.InlineError("Maximum amount is ₿10,000"),
            submitAmount(step, capacityMsat, onchainSats),
        )
    }

    @Test
    fun lnurlWithinBoundsFetchesInvoiceInMsat() {
        val lnurl = lnurlPayView(minSats = 1uL, maxSats = 10_000uL)
        val step = SendStep.Amount(
            target = classifiedView(kind = ClassifiedKind.LNURL),
            rawInput = "satoshi@zinqq.app",
            lnurl = lnurl,
            digits = "42",
        )
        val decision = submitAmount(step, capacityMsat, onchainSats)
        val fetch = assertIs<SendDecision.FetchLnurlInvoice>(decision)
        assertEquals(42_000uL, fetch.amountMsat)
        assertEquals(step, fetch.returnTo)
    }

    @Test
    fun onchainAmountBelowDustGates() {
        val step = SendStep.Amount(
            target = classifiedView(kind = ClassifiedKind.ONCHAIN, address = TEST_ADDRESS),
            rawInput = TEST_ADDRESS,
            digits = "293",
        )
        assertEquals(
            SendDecision.InlineError("Amount must be at least ₿294 (dust limit)"),
            submitAmount(step, capacityMsat, onchainSats),
        )
    }

    @Test
    fun onchainDustFloorPassesAtExactly294() {
        val step = SendStep.Amount(
            target = classifiedView(kind = ClassifiedKind.ONCHAIN, address = TEST_ADDRESS),
            rawInput = TEST_ADDRESS,
            digits = "294",
        )
        assertEquals(
            SendDecision.EstimateOnchain(TEST_ADDRESS, 294uL, returnTo = step),
            submitAmount(step, capacityMsat, onchainSats),
        )
    }

    @Test
    fun onchainSendMaxSkipsDustGateAndAsksForDrainEstimate() {
        val step = SendStep.Amount(
            target = classifiedView(kind = ClassifiedKind.ONCHAIN, address = TEST_ADDRESS),
            rawInput = TEST_ADDRESS,
            digits = "1", // stale prefill; the estimate owns the real amount
            isSendMax = true,
        )
        assertEquals(
            SendDecision.EstimateOnchainMax(TEST_ADDRESS, returnTo = step),
            submitAmount(step, capacityMsat, onchainSats),
        )
    }

    @Test
    fun amountlessBolt11OverCapacityGatesAtAmountStep() {
        val step = SendStep.Amount(
            target = classifiedView(kind = ClassifiedKind.BOLT11, bolt11 = TEST_BOLT11),
            rawInput = TEST_BOLT11,
            digits = "1001", // 1,001,000 msat > 1,000,000 msat capacity
        )
        assertEquals(
            SendDecision.InlineError("Not enough funds"),
            submitAmount(step, capacityMsat, onchainSats),
        )
    }

    @Test
    fun amountlessBolt11WithinCapacityReviewsWithReturnPath() {
        val step = SendStep.Amount(
            target = classifiedView(kind = ClassifiedKind.BOLT11, bolt11 = TEST_BOLT11),
            rawInput = TEST_BOLT11,
            digits = "1000",
        )
        val review = assertIs<SendStep.ReviewLightning>(
            assertIs<SendDecision.Step>(submitAmount(step, capacityMsat, onchainSats)).step,
        )
        assertEquals(1_000_000uL, review.amountMsat)
        assertEquals(step, review.returnTo)
        // From the amount step the raw input itself is the label (PWA :594).
        assertEquals(TEST_BOLT11, review.recipient)
    }

    // --- Lightning available prefill ---

    @Test
    fun lightningPrefillCapsAtLnurlMax() {
        assertEquals(5_000uL, lnAvailablePrefillSats(20_000uL, 5_000uL))
        assertEquals(20_000uL, lnAvailablePrefillSats(20_000uL, 50_000uL))
        assertEquals(20_000uL, lnAvailablePrefillSats(20_000uL, null))
    }

    @Test
    fun unifiedTotalFloorsLightningMsat() {
        assertEquals(10_001uL, unifiedTotalSats(10_000uL, 1_999uL))
    }

    // --- review derivation (fees / totals) ---

    @Test
    fun exactAmountReviewDerivesFeeRowsAndTotal() {
        val review = onchainReview(
            address = TEST_ADDRESS,
            amountSats = 5_000uL,
            estimate = feeEstimate(feeSats = 420uL, feeRateSatPerVb = 3uL),
            returnTo = null,
        )
        assertEquals(5_000uL, review.amountSats)
        assertEquals(420uL, review.feeSats)
        assertEquals(3uL, review.feeRateSatPerVb)
        assertEquals(5_420uL, review.totalSats)
        assertEquals(0uL, review.reserveSats)
        assertTrue(!review.isSendMax)
    }

    @Test
    fun sendMaxReviewCarriesTheReserve() {
        val review = onchainMaxReview(
            address = TEST_ADDRESS,
            estimate = maxSendEstimate(
                amountSats = 39_500uL,
                feeSats = 500uL,
                reserveSats = 10_000uL,
            ),
            returnTo = null,
        )
        assertEquals(39_500uL, review.amountSats)
        assertEquals(10_000uL, review.reserveSats)
        assertEquals(40_000uL, review.totalSats)
        assertTrue(review.isSendMax)
    }

    @Test
    fun lnurlInvoiceReviewPrefersTheInvoiceAmount() {
        val invoice = classifiedView(
            kind = ClassifiedKind.BOLT11,
            bolt11 = TEST_BOLT11,
            amountMsat = 42_000uL,
        )
        val review = lnurlInvoiceReview(invoice, 42_000uL, "satoshi@zinqq.app", returnTo = null)
        assertEquals(42_000uL, review.amountMsat)
        assertEquals("satoshi@zinqq.app", review.recipient)
    }

    // --- drift guard (R5) ---

    @Test
    fun driftRefreshOnMaxPathSwapsFiguresAndRaisesBanner() {
        val review = onchainMaxReview(TEST_ADDRESS, maxSendEstimate(), returnTo = null)
            .copy(broadcasting = true)
        val refreshed = refreshedMaxReview(
            review,
            maxSendEstimate(amountSats = 39_000uL, feeSats = 1_000uL, reserveSats = 10_000uL),
        )
        assertEquals(39_000uL, refreshed.amountSats)
        assertEquals(1_000uL, refreshed.feeSats)
        assertTrue(refreshed.amountsUpdated)
        assertTrue(!refreshed.broadcasting)
    }

    @Test
    fun driftRefreshOnExactPathKeepsTheAmount() {
        val review = onchainReview(TEST_ADDRESS, 5_000uL, feeEstimate(feeSats = 400uL), null)
        val refreshed = refreshedExactReview(review, feeEstimate(feeSats = 800uL))
        assertEquals(5_000uL, refreshed.amountSats)
        assertEquals(800uL, refreshed.feeSats)
        assertTrue(refreshed.amountsUpdated)
    }

    // --- outcome events + timeout ---

    @Test
    fun paymentSuccessfulSettlesToSuccessWithCeilSats() {
        val dispatching =
            SendStep.Dispatching(amountMsat = 1_001uL, paymentHash = TEST_PAYMENT_HASH)
        val settled = applyOutcome(
            dispatching,
            Event.PaymentSuccessful(paymentHash = TEST_PAYMENT_HASH, feePaidMsat = null),
        )
        assertEquals(SendStep.Success(amountSats = 2uL), settled)
    }

    @Test
    fun paymentFailedSettlesToFailureWithReason() {
        val settled = applyOutcome(
            SendStep.Dispatching(1_000uL, TEST_PAYMENT_HASH),
            Event.PaymentFailed(paymentHash = TEST_PAYMENT_HASH, reason = "no route"),
        )
        assertEquals(SendStep.Failure(message = "no route"), settled)
    }

    @Test
    fun unrelatedEventsDoNotSettleTheDispatch() {
        assertNull(applyOutcome(SendStep.Dispatching(1_000uL), Event.SyncCompleted))
        assertTrue(isPaymentOutcome(Event.PaymentFailed(null, "x"), null))
        assertTrue(!isPaymentOutcome(Event.NodeStarted, null))
    }

    // --- F1: an outcome only settles the dispatch whose hash it carries ---

    @Test
    fun anotherPaymentsOutcomeNeverSettlesOurDispatch() {
        val dispatching = SendStep.Dispatching(1_000uL, TEST_PAYMENT_HASH)
        // A previous send that outlived the 5-minute cap is still in flight;
        // its success must not tell this user their payment went through.
        assertNull(
            applyOutcome(
                dispatching,
                Event.PaymentSuccessful(paymentHash = OTHER_PAYMENT_HASH, feePaidMsat = 1uL),
            ),
        )
        assertNull(
            applyOutcome(
                dispatching,
                Event.PaymentFailed(paymentHash = OTHER_PAYMENT_HASH, reason = "no route"),
            ),
        )
        // And the controller's await predicate keeps waiting on both.
        assertTrue(
            !isPaymentOutcome(
                Event.PaymentSuccessful(OTHER_PAYMENT_HASH, null),
                TEST_PAYMENT_HASH,
            ),
        )
        assertTrue(
            !isPaymentOutcome(Event.PaymentFailed(OTHER_PAYMENT_HASH, "x"), TEST_PAYMENT_HASH),
        )
        // A hashless failure belongs to a BOLT12 request, never to our BOLT11.
        assertTrue(!isPaymentOutcome(Event.PaymentFailed(null, "x"), TEST_PAYMENT_HASH))
    }

    @Test
    fun bolt12DispatchKeepsFirstOutcomeMatching() {
        // No hash exists before the invoice request, so any outcome settles.
        val dispatching = SendStep.Dispatching(2_000uL, paymentHash = null)
        assertTrue(isPaymentOutcome(Event.PaymentSuccessful(OTHER_PAYMENT_HASH, null), null))
        assertEquals(
            SendStep.Success(amountSats = 2uL),
            applyOutcome(dispatching, Event.PaymentSuccessful(OTHER_PAYMENT_HASH, null)),
        )
        assertEquals(
            SendStep.Failure(message = "no route"),
            applyOutcome(dispatching, Event.PaymentFailed(null, "no route")),
        )
    }

    @Test
    fun bolt11ReviewCarriesTheCoresPaymentHashAndBolt12DoesNot() {
        val bolt11 = classifiedView(
            kind = ClassifiedKind.BOLT11,
            bolt11 = TEST_BOLT11,
            paymentHash = TEST_PAYMENT_HASH,
            amountMsat = 50_000uL,
        )
        val review = assertIs<SendStep.ReviewLightning>(
            assertIs<SendDecision.Step>(route(TEST_BOLT11, bolt11)).step,
        )
        assertEquals(TEST_PAYMENT_HASH, review.paymentHash)

        val offer = classifiedView(
            kind = ClassifiedKind.BOLT12,
            offer = TEST_OFFER,
            amountMsat = 50_000uL,
        )
        val offerReview = assertIs<SendStep.ReviewLightning>(
            assertIs<SendDecision.Step>(route(TEST_OFFER, offer)).step,
        )
        assertNull(offerReview.paymentHash)
    }

    @Test
    fun lnurlFetchedInvoiceCarriesTheFetchedInvoicesHash() {
        val fetched = classifiedView(
            kind = ClassifiedKind.BOLT11,
            bolt11 = TEST_BOLT11,
            paymentHash = TEST_PAYMENT_HASH,
            amountMsat = 21_000uL,
        )
        val review = lnurlInvoiceReview(fetched, 21_000uL, "satoshi@zinqq.app", returnTo = null)
        assertEquals(TEST_PAYMENT_HASH, review.paymentHash)
    }

    @Test
    fun outcomeTimeoutIsANeutralTerminalState() {
        assertEquals(
            SendStep.TimedOut(amountMsat = 7_000uL),
            outcomeTimedOut(SendStep.Dispatching(7_000uL)),
        )
        assertEquals(5L * 60 * 1_000, SEND_OUTCOME_TIMEOUT_MS)
    }

    // --- error copy mapping (PWA taxonomy) ---

    @Test
    fun guardErrorsMapToThePwaCopy() {
        assertEquals(
            "Network fees are too high right now — try again later.",
            walletErrorMessage(WalletException.OnchainFeesTooHigh()),
        )
        assertEquals(
            "Balance too low to cover fees",
            walletErrorMessage(WalletException.OnchainBalanceTooLow()),
        )
        assertEquals(
            "This address is for a different Bitcoin network",
            walletErrorMessage(WalletException.WrongAddressNetwork()),
        )
        assertEquals(
            "Invalid Bitcoin address",
            walletErrorMessage(WalletException.InvalidAddress("script parse")),
        )
        assertEquals(
            "Amount is below the minimum for this address",
            walletErrorMessage(WalletException.OnchainAmountBelowDust(546uL)),
        )
        assertEquals("no route", walletErrorMessage(WalletException.SendFailed("no route")))
    }

    @Test
    fun onlyBalanceAndFeeGuardsReturnToTheAmountStep() {
        assertTrue(isAmountStepGuardError(WalletException.OnchainBalanceTooLow()))
        assertTrue(isAmountStepGuardError(WalletException.OnchainFeesTooHigh()))
        assertTrue(!isAmountStepGuardError(WalletException.OnchainAmountChanged()))
        assertTrue(!isAmountStepGuardError(WalletException.NotRunning()))
    }
}
