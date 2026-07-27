import Shared
import XCTest

@testable import iosApp

/// The send machine's step-transition matrix (U20): every classification
/// kind routes exactly like the PWA's `Send.tsx`, the gates carry its copy,
/// and the drift/timeout re-renders are pure data transforms — the SAME
/// fixtures as Android's `SendFlowTest`.
final class SendFlowTests: XCTestCase {

    private let capacityMsat: UInt64 = 1_000_000 // ₿1,000 outbound
    private let onchainSats: UInt64 = 50_000

    private func route(
        _ raw: String,
        _ view: ClassifiedView,
        lnurl: LnurlPayView? = nil
    ) -> SendDecision {
        routeInput(
            raw: raw,
            view: view,
            lnurl: lnurl,
            lnCapacityMsat: capacityMsat,
            onchainBalanceSats: onchainSats
        )
    }

    // MARK: input → routing per kind

    func testInvalidInputShowsTheCoreErrorVerbatim() {
        let decision = route(
            "junk",
            classifiedView(kind: .invalid, error: "Unrecognized payment format")
        )
        XCTAssertEqual(.inlineError("Unrecognized payment format"), decision)
    }

    func testBip353NameRequiresResolution() {
        let decision = route(
            "satoshi@zinqq.app",
            classifiedView(kind: .bip353, bip353User: "satoshi", bip353Domain: "zinqq.app")
        )
        XCTAssertEqual(.resolve(input: "satoshi@zinqq.app"), decision)
    }

    func testFixedAmountBolt11GoesStraightToReview() throws {
        let view = classifiedView(
            kind: .bolt11,
            bolt11: testBolt11,
            amountMsat: 50_000,
            description: "coffee"
        )
        guard case let .step(.reviewLightning(review)) = route(testBolt11, view) else {
            return XCTFail("expected a Lightning review step")
        }
        XCTAssertEqual(50_000, review.amountMsat)
        XCTAssertEqual("coffee", review.recipient)
        XCTAssertNil(review.returnTo)
    }

    func testFixedAmountBolt11OverCapacityGates() {
        let view = classifiedView(
            kind: .bolt11,
            bolt11: testBolt11,
            amountMsat: capacityMsat + 1
        )
        XCTAssertEqual(.inlineError("Not enough funds"), route(testBolt11, view))
    }

    func testAmountlessBolt11EntersAmountStep() throws {
        let view = classifiedView(kind: .bolt11, bolt11: testBolt11)
        guard case let .step(.amount(amount)) = route(testBolt11, view) else {
            return XCTFail("expected the amount step")
        }
        XCTAssertEqual(testBolt11, amount.rawInput)
        XCTAssertNil(amount.minSats)
    }

    func testFixedAmountBolt12GoesStraightToReview() throws {
        let view = classifiedView(kind: .bolt12, offer: testOffer, amountMsat: 21_000)
        guard case let .step(.reviewLightning(review)) = route(testOffer, view) else {
            return XCTFail("expected a Lightning review step")
        }
        XCTAssertEqual(21_000, review.amountMsat)
    }

    func testAmountlessBolt12EntersAmountStep() {
        let view = classifiedView(kind: .bolt12, offer: testOffer)
        guard case .step(.amount) = route(testOffer, view) else {
            return XCTFail("expected the amount step")
        }
    }

    func testLnurlWithRangeEntersAmountStepWithBounds() throws {
        let lnurl = lnurlPayView(minSats: 10, maxSats: 5_000)
        let view = classifiedView(kind: .lnurl, description: lnurl.description_)
        guard case let .step(.amount(amount)) = route("satoshi@zinqq.app", view, lnurl: lnurl)
        else {
            return XCTFail("expected the amount step")
        }
        XCTAssertEqual(10, amount.minSats)
        XCTAssertEqual(5_000, amount.maxSats)
    }

    func testLnurlMinEqualsMaxSkipsAmountEntry() throws {
        let lnurl = lnurlPayView(
            minSendableMsat: 5_000_000,
            maxSendableMsat: 5_000_000,
            minSats: 5_000,
            maxSats: 5_000,
            skipAmountEntry: true
        )
        let view = classifiedView(kind: .lnurl)
        guard case let .fetchLnurlInvoice(_, amountMsat, _, returnTo) =
            route("satoshi@zinqq.app", view, lnurl: lnurl)
        else {
            return XCTFail("expected a fetch-invoice decision")
        }
        XCTAssertEqual(5_000_000, amountMsat)
        XCTAssertNil(returnTo)
    }

    func testOnchainWithoutAmountEntersAmountStep() {
        let view = classifiedView(kind: .onchain, address: testAddress)
        guard case .step(.amount) = route(testAddress, view) else {
            return XCTFail("expected the amount step")
        }
    }

    func testOnchainEmbeddedAmountRequestsEstimate() {
        let view = classifiedView(kind: .onchain, address: testAddress, amountSats: 1_000)
        XCTAssertEqual(
            .estimateOnchain(address: testAddress, amountSats: 1_000, returnTo: nil),
            route("bitcoin:\(testAddress)?amount=0.00001", view)
        )
    }

    func testOnchainEmbeddedAmountBelowDustGates() {
        let view = classifiedView(kind: .onchain, address: testAddress, amountSats: 293)
        XCTAssertEqual(
            .inlineError("Amount must be at least ₿294 (dust limit)"),
            route(testAddress, view)
        )
    }

    func testOnchainEmbeddedAmountOverBalanceGates() {
        let view = classifiedView(
            kind: .onchain,
            address: testAddress,
            amountSats: onchainSats + 1
        )
        XCTAssertEqual(
            .inlineError("Amount exceeds available on-chain balance"),
            route(testAddress, view)
        )
    }

    // MARK: recipient labels (PWA Send.tsx:130-136, 469-471, 592-594)

    func testBip321WrappedInvoiceShowsTruncatedInvoiceLabel() throws {
        let view = classifiedView(
            kind: .bolt11,
            bolt11: testBolt11,
            amountMsat: 1_000,
            description: "ignored"
        )
        guard case let .step(.reviewLightning(review)) =
            route("bitcoin:\(testAddress)?lightning=x", view)
        else {
            return XCTFail("expected a Lightning review step")
        }
        XCTAssertEqual(String(testBolt11.prefix(10)) + "…", review.recipient)
    }

    func testResolvedNameShowsTheNameAsRecipient() throws {
        let view = classifiedView(kind: .bolt12, offer: testOffer, amountMsat: 1_000)
        guard case let .step(.reviewLightning(review)) = route("satoshi@zinqq.app", view) else {
            return XCTFail("expected a Lightning review step")
        }
        XCTAssertEqual("satoshi@zinqq.app", review.recipient)
    }

    func testPlainInvoiceWithoutDescriptionShowsTruncation() throws {
        let view = classifiedView(kind: .bolt11, bolt11: testBolt11, amountMsat: 1_000)
        guard case let .step(.reviewLightning(review)) = route(testBolt11, view) else {
            return XCTFail("expected a Lightning review step")
        }
        XCTAssertEqual(truncateInvoice(testBolt11), review.recipient)
    }

    // MARK: amount step: numpad + gates

    func testNumpadKeyResetsErrorAndSendMax() {
        let step = SendAmountStep(
            target: classifiedView(kind: .onchain, address: testAddress),
            rawInput: testAddress,
            digits: "10",
            isSendMax: true,
            error: "old"
        )
        let next = reduceAmountKey(step, key: .digit("5"))
        XCTAssertEqual("105", next.digits)
        XCTAssertFalse(next.isSendMax)
        XCTAssertNil(next.error)
    }

    func testLnurlBelowMinimumGatesWithPwaCopy() {
        let step = SendAmountStep(
            target: classifiedView(kind: .lnurl),
            rawInput: "satoshi@zinqq.app",
            lnurl: lnurlPayView(minSats: 1_000, maxSats: 10_000),
            digits: "999"
        )
        XCTAssertEqual(
            .inlineError("Minimum amount is ₿1,000"),
            submitAmount(step, lnCapacityMsat: capacityMsat, onchainBalanceSats: onchainSats)
        )
    }

    func testLnurlAboveMaximumGatesWithPwaCopy() {
        let step = SendAmountStep(
            target: classifiedView(kind: .lnurl),
            rawInput: "satoshi@zinqq.app",
            lnurl: lnurlPayView(minSats: 1, maxSats: 10_000),
            digits: "10001"
        )
        XCTAssertEqual(
            .inlineError("Maximum amount is ₿10,000"),
            submitAmount(step, lnCapacityMsat: capacityMsat, onchainBalanceSats: onchainSats)
        )
    }

    func testLnurlWithinBoundsFetchesInvoiceInMsat() throws {
        let lnurl = lnurlPayView(minSats: 1, maxSats: 10_000)
        let step = SendAmountStep(
            target: classifiedView(kind: .lnurl),
            rawInput: "satoshi@zinqq.app",
            lnurl: lnurl,
            digits: "42"
        )
        guard case let .fetchLnurlInvoice(_, amountMsat, _, returnTo) =
            submitAmount(step, lnCapacityMsat: capacityMsat, onchainBalanceSats: onchainSats)
        else {
            return XCTFail("expected a fetch-invoice decision")
        }
        XCTAssertEqual(42_000, amountMsat)
        XCTAssertEqual(step, returnTo)
    }

    func testOnchainAmountBelowDustGates() {
        let step = SendAmountStep(
            target: classifiedView(kind: .onchain, address: testAddress),
            rawInput: testAddress,
            digits: "293"
        )
        XCTAssertEqual(
            .inlineError("Amount must be at least ₿294 (dust limit)"),
            submitAmount(step, lnCapacityMsat: capacityMsat, onchainBalanceSats: onchainSats)
        )
    }

    func testOnchainDustFloorPassesAtExactly294() {
        let step = SendAmountStep(
            target: classifiedView(kind: .onchain, address: testAddress),
            rawInput: testAddress,
            digits: "294"
        )
        XCTAssertEqual(
            .estimateOnchain(address: testAddress, amountSats: 294, returnTo: step),
            submitAmount(step, lnCapacityMsat: capacityMsat, onchainBalanceSats: onchainSats)
        )
    }

    func testOnchainSendMaxSkipsDustGateAndAsksForDrainEstimate() {
        let step = SendAmountStep(
            target: classifiedView(kind: .onchain, address: testAddress),
            rawInput: testAddress,
            digits: "1", // stale prefill; the estimate owns the real amount
            isSendMax: true
        )
        XCTAssertEqual(
            .estimateOnchainMax(address: testAddress, returnTo: step),
            submitAmount(step, lnCapacityMsat: capacityMsat, onchainBalanceSats: onchainSats)
        )
    }

    func testAmountlessBolt11OverCapacityGatesAtAmountStep() {
        let step = SendAmountStep(
            target: classifiedView(kind: .bolt11, bolt11: testBolt11),
            rawInput: testBolt11,
            digits: "1001" // 1,001,000 msat > 1,000,000 msat capacity
        )
        XCTAssertEqual(
            .inlineError("Not enough funds"),
            submitAmount(step, lnCapacityMsat: capacityMsat, onchainBalanceSats: onchainSats)
        )
    }

    func testAmountlessBolt11WithinCapacityReviewsWithReturnPath() throws {
        let step = SendAmountStep(
            target: classifiedView(kind: .bolt11, bolt11: testBolt11),
            rawInput: testBolt11,
            digits: "1000"
        )
        guard case let .step(.reviewLightning(review)) =
            submitAmount(step, lnCapacityMsat: capacityMsat, onchainBalanceSats: onchainSats)
        else {
            return XCTFail("expected a Lightning review step")
        }
        XCTAssertEqual(1_000_000, review.amountMsat)
        XCTAssertEqual(step, review.returnTo)
        // From the amount step the raw input itself is the label (PWA :594).
        XCTAssertEqual(testBolt11, review.recipient)
    }

    // MARK: Lightning available prefill

    func testLightningPrefillCapsAtLnurlMax() {
        XCTAssertEqual(5_000, lnAvailablePrefillSats(unifiedTotalSats: 20_000, maxSats: 5_000))
        XCTAssertEqual(20_000, lnAvailablePrefillSats(unifiedTotalSats: 20_000, maxSats: 50_000))
        XCTAssertEqual(20_000, lnAvailablePrefillSats(unifiedTotalSats: 20_000, maxSats: nil))
    }

    func testUnifiedTotalFloorsLightningMsat() {
        XCTAssertEqual(10_001, unifiedTotalSats(onchainBalanceSats: 10_000, lightningMsat: 1_999))
    }

    // MARK: review derivation (fees / totals)

    func testExactAmountReviewDerivesFeeRowsAndTotal() {
        let review = onchainReview(
            address: testAddress,
            amountSats: 5_000,
            estimate: feeEstimate(feeSats: 420, feeRateSatPerVb: 3),
            returnTo: nil
        )
        XCTAssertEqual(5_000, review.amountSats)
        XCTAssertEqual(420, review.feeSats)
        XCTAssertEqual(3, review.feeRateSatPerVb)
        XCTAssertEqual(5_420, review.totalSats)
        XCTAssertEqual(0, review.reserveSats)
        XCTAssertFalse(review.isSendMax)
    }

    func testSendMaxReviewCarriesTheReserve() {
        let review = onchainMaxReview(
            address: testAddress,
            estimate: maxSendEstimate(amountSats: 39_500, feeSats: 500, reserveSats: 10_000),
            returnTo: nil
        )
        XCTAssertEqual(39_500, review.amountSats)
        XCTAssertEqual(10_000, review.reserveSats)
        XCTAssertEqual(40_000, review.totalSats)
        XCTAssertTrue(review.isSendMax)
    }

    func testLnurlInvoiceReviewPrefersTheInvoiceAmount() {
        let invoice = classifiedView(kind: .bolt11, bolt11: testBolt11, amountMsat: 42_000)
        let review = lnurlInvoiceReview(
            invoice: invoice,
            requestedMsat: 42_000,
            rawInput: "satoshi@zinqq.app",
            returnTo: nil
        )
        XCTAssertEqual(42_000, review.amountMsat)
        XCTAssertEqual("satoshi@zinqq.app", review.recipient)
    }

    // MARK: drift guard (R5)

    func testDriftRefreshOnMaxPathSwapsFiguresAndRaisesBanner() {
        var review = onchainMaxReview(
            address: testAddress, estimate: maxSendEstimate(), returnTo: nil
        )
        review.broadcasting = true
        let refreshed = refreshedMaxReview(
            review,
            fresh: maxSendEstimate(amountSats: 39_000, feeSats: 1_000, reserveSats: 10_000)
        )
        XCTAssertEqual(39_000, refreshed.amountSats)
        XCTAssertEqual(1_000, refreshed.feeSats)
        XCTAssertTrue(refreshed.amountsUpdated)
        XCTAssertFalse(refreshed.broadcasting)
    }

    func testDriftRefreshOnExactPathKeepsTheAmount() {
        let review = onchainReview(
            address: testAddress,
            amountSats: 5_000,
            estimate: feeEstimate(feeSats: 400),
            returnTo: nil
        )
        let refreshed = refreshedExactReview(review, fresh: feeEstimate(feeSats: 800))
        XCTAssertEqual(5_000, refreshed.amountSats)
        XCTAssertEqual(800, refreshed.feeSats)
        XCTAssertTrue(refreshed.amountsUpdated)
    }

    // MARK: outcome events + timeout

    func testPaymentSuccessfulSettlesToSuccessWithCeilSats() {
        let settled = applyOutcome(amountMsat: 1_001, event: .paymentSuccessful)
        XCTAssertEqual(.success(amountSats: 2, txid: nil), settled)
    }

    func testPaymentFailedSettlesToFailureWithReason() {
        let settled = applyOutcome(amountMsat: 1_000, event: .paymentFailed(reason: "no route"))
        XCTAssertEqual(.failure(message: "no route", retry: nil), settled)
    }

    func testUnrelatedEventsDoNotSettleTheDispatch() {
        XCTAssertNil(applyOutcome(amountMsat: 1_000, event: .syncCompleted))
        XCTAssertTrue(isPaymentOutcome(.paymentFailed(reason: "x")))
        XCTAssertFalse(isPaymentOutcome(.nodeStarted))
    }

    func testOutcomeTimeoutIsANeutralTerminalState() {
        XCTAssertEqual(.timedOut(amountMsat: 7_000), outcomeTimedOut(amountMsat: 7_000))
        XCTAssertEqual(5 * 60 * 1_000, sendOutcomeTimeoutMs)
    }

    // MARK: error copy mapping (PWA taxonomy)

    func testGuardErrorsMapToThePwaCopy() {
        XCTAssertEqual(
            "Network fees are too high right now — try again later.",
            walletErrorMessage(WalletException.OnchainFeesTooHigh())
        )
        XCTAssertEqual(
            "Balance too low to cover fees",
            walletErrorMessage(WalletException.OnchainBalanceTooLow())
        )
        XCTAssertEqual(
            "This address is for a different Bitcoin network",
            walletErrorMessage(WalletException.WrongAddressNetwork())
        )
        XCTAssertEqual(
            "Invalid Bitcoin address",
            walletErrorMessage(WalletException.InvalidAddress(detail: "script parse"))
        )
        XCTAssertEqual(
            "Amount is below the minimum for this address",
            walletErrorMessage(WalletException.OnchainAmountBelowDust(minSats: 546))
        )
        XCTAssertEqual(
            "no route",
            walletErrorMessage(WalletException.SendFailed(reason: "no route"))
        )
    }

    func testOnlyBalanceAndFeeGuardsReturnToTheAmountStep() {
        XCTAssertTrue(isAmountStepGuardError(WalletException.OnchainBalanceTooLow()))
        XCTAssertTrue(isAmountStepGuardError(WalletException.OnchainFeesTooHigh()))
        XCTAssertFalse(isAmountStepGuardError(WalletException.OnchainAmountChanged()))
        XCTAssertFalse(isAmountStepGuardError(WalletException.NotRunning()))
    }
}
