import Shared
import XCTest

@testable import iosApp

/// The receive machine's gating and transition matrices (U21): floor gating
/// (AE4), the needs-JIT presentation decision, quote staleness → re-quote,
/// expiry flip + mid-edit suppression, received settle, pager eligibility,
/// caption derivation, and countdown formatting — the SAME fixtures as
/// Android's `ReceiveFlowTest` (mirroring the PWA's `Receive.tsx` /
/// `Receive.test.tsx`).
final class ReceiveFlowTests: XCTestCase {

    private let floor: UInt64 = 3_000

    // MARK: floor gating matrix (AE4, PWA Receive.tsx:133-134, test:576-604)

    func testBelowFloorJitAmountIsBlocked() {
        // No capacity: every sat needs JIT.
        XCTAssertTrue(
            belowJitMinimum(
                needsJit: editingNeedsJit(usableInboundMsat: 0, amountSats: 2_999),
                amountSats: 2_999,
                floorSats: floor
            )
        )
        XCTAssertEqual(
            .blocked,
            confirmAmountDecision(amountSats: 2_999, usableInboundMsat: 0, floorSats: floor)
        )
    }

    func testAtFloorJitAmountPasses() {
        XCTAssertFalse(
            belowJitMinimum(
                needsJit: editingNeedsJit(usableInboundMsat: 0, amountSats: 3_000),
                amountSats: 3_000,
                floorSats: floor
            )
        )
        XCTAssertEqual(
            .request(amountSats: 3_000, presentQuoting: true),
            confirmAmountDecision(amountSats: 3_000, usableInboundMsat: 0, floorSats: floor)
        )
    }

    func testAboveFloorJitAmountPasses() {
        XCTAssertEqual(
            .request(amountSats: 50_000, presentQuoting: true),
            confirmAmountDecision(amountSats: 50_000, usableInboundMsat: 0, floorSats: floor)
        )
    }

    func testBelowFloorAmountCoveredByCapacityIsNotGated() {
        // In-capacity receives are unaffected by the JIT floor (AE4 scope).
        let inbound: UInt64 = 10_000 * 1_000
        XCTAssertFalse(
            belowJitMinimum(
                needsJit: editingNeedsJit(usableInboundMsat: inbound, amountSats: 500),
                amountSats: 500,
                floorSats: floor
            )
        )
        XCTAssertEqual(
            .request(amountSats: 500, presentQuoting: false),
            confirmAmountDecision(amountSats: 500, usableInboundMsat: inbound, floorSats: floor)
        )
    }

    func testZeroAmountRaisesNoAlertAndDisablesNext() {
        XCTAssertFalse(
            belowJitMinimum(
                needsJit: editingNeedsJit(usableInboundMsat: 0, amountSats: 0),
                amountSats: 0,
                floorSats: floor
            )
        )
        XCTAssertFalse(numpadNextEnabled(amountSats: 0, belowMinimum: false))
    }

    func testBelowMinimumDisablesNext() {
        XCTAssertFalse(numpadNextEnabled(amountSats: 2_999, belowMinimum: true))
        XCTAssertTrue(numpadNextEnabled(amountSats: 3_000, belowMinimum: false))
    }

    func testMinimumAlertCarriesThePwaCopy() {
        XCTAssertEqual("Minimum ₿3,000", minimumAlertText(floorSats: 3_000))
    }

    func testLiveFloorRaisesTheGateAboveTheStaticFloor() {
        // PWA test:650: live LSP minimum above the static floor governs.
        let liveFloor: UInt64 = 5_000
        XCTAssertTrue(
            belowJitMinimum(
                needsJit: editingNeedsJit(usableInboundMsat: 0, amountSats: 4_000),
                amountSats: 4_000,
                floorSats: liveFloor
            )
        )
    }

    // MARK: needs-JIT decision presentation (PWA Receive.tsx:425-439)

    func testJitConfirmPresentsTheQuotingSkeletonImmediately() {
        // Inbound covers 5,000 sats; asking 6,000 needs JIT.
        XCTAssertEqual(
            .request(amountSats: 6_000, presentQuoting: true),
            confirmAmountDecision(
                amountSats: 6_000, usableInboundMsat: 5_000_000, floorSats: floor
            )
        )
    }

    func testInCapacityConfirmDoesNotPresentQuoting() {
        XCTAssertEqual(
            .request(amountSats: 5_000, presentQuoting: false),
            confirmAmountDecision(
                amountSats: 5_000, usableInboundMsat: 5_000_000, floorSats: floor
            )
        )
    }

    func testExactCapacityBoundaryNeedsJit() {
        // needs_jit is `inbound < amount * 1000` — equality is servable.
        XCTAssertFalse(editingNeedsJit(usableInboundMsat: 5_000_000, amountSats: 5_000))
        XCTAssertTrue(editingNeedsJit(usableInboundMsat: 4_999_999, amountSats: 5_000))
    }

    // MARK: usable inbound sum (PWA Receive.tsx:33-39)

    func testUsableInboundSumsOnlyUsableChannels() {
        let channels = [
            makeChannel(inboundMsat: 1_000_000, usable: true),
            makeChannel(inboundMsat: 2_000_000, usable: false),
            makeChannel(inboundMsat: 3_000_000, usable: true),
        ]
        XCTAssertEqual(4_000_000, sumUsableInboundMsat(channels))
        XCTAssertEqual(0, sumUsableInboundMsat([]))
    }

    // MARK: review derivation (PWA Receive.tsx:726-751)

    func testReviewRowsCeilTheFeeAndDeriveTheNet() {
        let review = ReceiveJitReview(
            amountSats: 10_000,
            quote: makeQuote(amountMsat: 10_000_000, openingFeeMsat: 2_500_001)
        )
        // (2_500_001 + 999) / 1000 = 2501 — the PWA's ceil.
        XCTAssertEqual(2_501, review.setupFeeSats)
        XCTAssertEqual(7_499, review.youReceiveSats)
    }

    // MARK: quote staleness → re-quote (PWA Receive.tsx:534-537)

    func testStaleQuoteAtBuyDemandsAReQuote() {
        XCTAssertEqual(.reQuote, classifyBuyFailure(WalletException.JitReQuoteRequired()))
        // And through the bridge's NSError wrapping, as the controller sees it.
        XCTAssertEqual(
            .reQuote,
            classifyBuyFailure(kotlinError(WalletException.JitReQuoteRequired()) as Error)
        )
    }

    func testOtherBuyFailuresGoToTheErrorScreen() {
        XCTAssertEqual(
            .error,
            classifyBuyFailure(WalletException.Lsps2(reason: "lsps2.buy failed: boom"))
        )
        XCTAssertEqual(
            .error,
            classifyBuyFailure(NSError(domain: "network", code: 1) as Error)
        )
    }

    // MARK: quote failure classification (PWA Receive.tsx:249-268)

    func testBelowMinimumQuoteFailureIsRecognisedFromTheCoreCopy() {
        let e = WalletException.Lsps2(
            reason: "LSPS2 request failed: amount 500000msat is below the LSP minimum "
                + "payment size of 3000000msat"
        )
        XCTAssertEqual(.belowMinimum(minPaymentSizeMsat: 3_000_000), classifyQuoteFailure(e))
        // And through the bridge's NSError wrapping, as the controller sees it.
        XCTAssertEqual(
            .belowMinimum(minPaymentSizeMsat: 3_000_000),
            classifyQuoteFailure(kotlinError(e) as Error)
        )
    }

    func testOtherQuoteFailuresFallBackToOnchain() {
        XCTAssertEqual(
            .other,
            classifyQuoteFailure(WalletException.Lsps2(reason: "lsps2.get_info failed: boom"))
        )
        XCTAssertEqual(
            .other,
            classifyQuoteFailure(NSError(domain: "nope", code: 1) as Error)
        )
    }

    // MARK: expiry flip + suppression (PWA Receive.tsx:319-330, 814-818)

    func testDisplayedJitInvoiceFlipsToExpired() {
        XCTAssertEqual(.jitExpired, applyExpiryFlip(.display(invoicePath: .jit)))
    }

    func testOnlyTheJitQrFlips() {
        let standard = ReceiveStep.display(invoicePath: .standard)
        XCTAssertEqual(standard, applyExpiryFlip(standard))
        let review = ReceiveStep.jitReview(
            ReceiveJitReview(amountSats: 10_000, quote: makeQuote())
        )
        XCTAssertEqual(review, applyExpiryFlip(review))
        let received = ReceiveStep.received(amountSats: 10_000)
        XCTAssertEqual(received, applyExpiryFlip(received))
    }

    func testExpiredScreenIsSuppressedMidEdit() {
        // PWA test:431: expiry mid-edit keeps the numpad; Cancel lands on it.
        XCTAssertFalse(showExpiredScreen(step: .jitExpired, editingAmount: true))
        XCTAssertTrue(showExpiredScreen(step: .jitExpired, editingAmount: false))
        XCTAssertFalse(
            showExpiredScreen(step: .display(invoicePath: .jit), editingAmount: false)
        )
    }

    func testCountdownIsSuppressedWhileEditing() {
        let jit = ReceiveStep.display(invoicePath: .jit)
        XCTAssertTrue(countdownVisible(step: jit, editingAmount: false, expiresAtUnix: 1_700))
        XCTAssertFalse(countdownVisible(step: jit, editingAmount: true, expiresAtUnix: 1_700))
        XCTAssertFalse(countdownVisible(step: jit, editingAmount: false, expiresAtUnix: nil))
        XCTAssertFalse(
            countdownVisible(
                step: .display(invoicePath: .standard),
                editingAmount: false,
                expiresAtUnix: 1_700
            )
        )
    }

    // MARK: countdown math + formatting (R6)

    func testCountdownIsExpiryMinusNowFlooredAtZero() {
        XCTAssertEqual(
            576, countdownSecondsLeft(expiresAtUnix: 1_700_000_576, nowUnixSecs: 1_700_000_000)
        )
        XCTAssertEqual(
            0, countdownSecondsLeft(expiresAtUnix: 1_700_000_000, nowUnixSecs: 1_700_000_000)
        )
        XCTAssertEqual(
            0, countdownSecondsLeft(expiresAtUnix: 1_699_999_000, nowUnixSecs: 1_700_000_000)
        )
    }

    func testCountdownFormatsMinutesAndPaddedSeconds() {
        XCTAssertEqual("Expires in 9:36", countdownText(secondsLeft: 576))
        XCTAssertEqual("Expires in 0:59", countdownText(secondsLeft: 59))
        XCTAssertEqual("Expires in 0:00", countdownText(secondsLeft: 0))
        XCTAssertEqual("Expires in 60:00", countdownText(secondsLeft: 3_600))
    }

    // MARK: received settle (PWA Receive.tsx:332-343)

    func testMatchingPaymentReceivedSettlesWithTheFlooredAmount() {
        let settled = applyPaymentReceived(
            awaitedPaymentHash: testPaymentHash,
            event: .paymentReceived(
                paymentHash: testPaymentHash,
                amountMsat: 12_345_678,
                skimmedFeeMsat: 2_500_000
            )
        )
        XCTAssertEqual(.received(amountSats: 12_345), settled)
    }

    func testMismatchedOrAbsentHashDoesNotSettle() {
        let event = WalletEvent.paymentReceived(
            paymentHash: "other", amountMsat: 1_000, skimmedFeeMsat: nil
        )
        XCTAssertNil(applyPaymentReceived(awaitedPaymentHash: testPaymentHash, event: event))
        XCTAssertNil(applyPaymentReceived(awaitedPaymentHash: nil, event: event))
    }

    func testNonReceiveEventsDoNotSettle() {
        XCTAssertNil(
            applyPaymentReceived(
                awaitedPaymentHash: testPaymentHash,
                event: .invoiceReady(bolt11: "lnbc1", expiryUnixSecs: 0)
            )
        )
    }

    // MARK: pager eligibility (PWA Receive.tsx:372, R6)

    func testOfferPageNeedsAnOfferAndAUsableChannel() {
        // The core only emits an offer with ≥1 usable channel, so
        // offerExists encodes that; needsAmount is the no-channel visit.
        XCTAssertTrue(showBolt12Page(offerExists: true, needsAmount: false))
        XCTAssertFalse(showBolt12Page(offerExists: false, needsAmount: false))
        XCTAssertFalse(showBolt12Page(offerExists: true, needsAmount: true))
        XCTAssertFalse(showBolt12Page(offerExists: false, needsAmount: true))
    }

    // MARK: caption + copy derivation (PWA Receive.tsx:993-1001, 1027-1029)

    func testCaptionsFollowThePageAndInvoicePath() {
        XCTAssertEqual(
            "Reusable QR code",
            qrCaption(page: .bolt12, invoicePath: .standard, openingFeeSats: nil)
        )
        XCTAssertEqual(
            "Setup fee: ₿2,500",
            qrCaption(page: .unified, invoicePath: .jit, openingFeeSats: 2_500)
        )
        XCTAssertEqual(
            "Request money by letting someone scan this QR code",
            qrCaption(page: .unified, invoicePath: .standard, openingFeeSats: nil)
        )
        XCTAssertEqual(
            "Request money by letting someone scan this QR code",
            qrCaption(page: .unified, invoicePath: .none, openingFeeSats: nil)
        )
    }

    func testCopySheetTitleFollowsThePage() {
        XCTAssertEqual("Reusable payment request", copySheetTitle(page: .bolt12))
        XCTAssertEqual("Payment request", copySheetTitle(page: .unified))
    }

    func testCopyValueUsesTheLowercaseLnoFormOnTheOfferPage() {
        let uri = "bitcoin:BC1Q?lightning=lnbc1"
        XCTAssertEqual(
            "bitcoin:?lno=\(testReceiveOffer)",
            copyValue(page: .bolt12, bip321Uri: uri, offer: testReceiveOffer)
        )
        XCTAssertEqual(uri, copyValue(page: .unified, bip321Uri: uri, offer: testReceiveOffer))
        // No offer: the bolt12 page cannot exist, fall back to the URI.
        XCTAssertEqual(uri, copyValue(page: .bolt12, bip321Uri: uri, offer: nil))
    }

    // MARK: numpad CTA + header copy (PWA Receive.tsx:928, 642-649)

    func testMandatoryFirstAmountUsesTheRequestCta() {
        XCTAssertEqual("Request", numpadCtaLabel(needsAmount: true, confirmedAmountSats: 0))
        XCTAssertEqual("Done", numpadCtaLabel(needsAmount: true, confirmedAmountSats: 500))
        XCTAssertEqual("Done", numpadCtaLabel(needsAmount: false, confirmedAmountSats: 0))
    }

    func testHeaderCopyOnlyShowsOverTheQr() {
        let display = ReceiveStep.display(invoicePath: .standard)
        XCTAssertTrue(headerCopyVisible(hasAddress: true, editingAmount: false, step: display))
        XCTAssertFalse(headerCopyVisible(hasAddress: false, editingAmount: false, step: display))
        XCTAssertFalse(headerCopyVisible(hasAddress: true, editingAmount: true, step: display))
        XCTAssertFalse(headerCopyVisible(hasAddress: true, editingAmount: false, step: .quoting))
        XCTAssertFalse(
            headerCopyVisible(
                hasAddress: true,
                editingAmount: false,
                step: .jitReview(ReceiveJitReview(amountSats: 1, quote: makeQuote()))
            )
        )
        XCTAssertFalse(headerCopyVisible(hasAddress: true, editingAmount: false, step: .buying))
        XCTAssertFalse(
            headerCopyVisible(hasAddress: true, editingAmount: false, step: .jitExpired)
        )
        XCTAssertFalse(
            headerCopyVisible(hasAddress: true, editingAmount: false, step: .jitError)
        )
    }

    // MARK: fixture sanity: the step type used by the controller matrix

    func testBundleFixtureCarriesTheJitDecision() {
        let bundle = makeBundle(bolt11: nil, paymentHash: nil, needsJit: true)
        XCTAssertTrue(bundle.needsJit)
        XCTAssertNil(bundle.bolt11)
        XCTAssertEqual(
            ReceiveStep.display(invoicePath: .none), .display(invoicePath: .none)
        )
    }
}
