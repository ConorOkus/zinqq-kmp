import Shared
import XCTest

@testable import iosApp

/// CloseChannel's pure derivations (U22, R10 UI): the coop/force confirm
/// variants, informational estimate rendering with every field independently
/// nullable (`CloseChannel.tsx:276-293`), the non-anchor and in-flight
/// warnings (`CloseChannel.tsx:399-413`), the success copy
/// (`CloseChannel.tsx:192-206`), and the force-close escalation offer
/// (`CloseChannel.tsx:139-146,239-252`) — Android's `CloseChannelLogicTest`
/// ported fixture-for-fixture.
final class CloseChannelLogicTests: XCTestCase {

    // MARK: estimated cost (CloseChannel.tsx:276-281)

    func testCostLabelUsesTheVariantTotal() {
        let estimate = closeEstimate(
            coopTotalYouPaySats: 300,
            forceTotalYouPaySats: 2_500
        )
        XCTAssertEqual("~₿300", closeCostLabel(estimate, force: false, loading: false))
        XCTAssertEqual("~₿2,500", closeCostLabel(estimate, force: true, loading: false))
    }

    func testCostLabelHandlesLoadingAndUnavailable() {
        XCTAssertEqual("Estimating…", closeCostLabel(nil, force: false, loading: true))
        XCTAssertEqual(
            "Estimate unavailable",
            closeCostLabel(closeEstimate(), force: false, loading: false)
        )
        XCTAssertEqual(
            "Estimate unavailable", closeCostLabel(nil, force: true, loading: false)
        )
    }

    // MARK: funds-available timeline (CloseChannel.tsx:282-286)

    func testCoopTimelineIsMinutes() {
        XCTAssertEqual(
            "~minutes once confirmed",
            closeTimelineLabel(closeEstimate(timelockBlocks: 144), force: false)
        )
    }

    func testForceTimelineHumanizesTheTimelock() {
        XCTAssertEqual(
            "up to ~24 hours",
            closeTimelineLabel(closeEstimate(timelockBlocks: 144), force: true)
        )
        XCTAssertEqual("up to ~14 days", closeTimelineLabel(closeEstimate(), force: true))
        XCTAssertEqual("up to ~14 days", closeTimelineLabel(nil, force: true))
    }

    // MARK: you-get-back (CloseChannel.tsx:287-291)

    func testExpectedBackRendersValueLoadingOrPlaceholder() {
        XCTAssertEqual(
            "~₿54,321",
            expectedBackLabel(closeEstimate(expectedBackSats: 54_321), loading: false)
        )
        XCTAssertEqual("Estimating…", expectedBackLabel(nil, loading: true))
        XCTAssertEqual("—", expectedBackLabel(closeEstimate(), loading: false))
    }

    // MARK: notes and warnings

    func testLspPaysNoteOnlyForCoopWithCounterpartyFunder() {
        let counterpartyFunded = closeEstimate(feePayer: .counterparty)
        XCTAssertTrue(lspPaysCloseFee(counterpartyFunded, force: false))
        XCTAssertFalse(lspPaysCloseFee(counterpartyFunded, force: true))
        XCTAssertFalse(lspPaysCloseFee(closeEstimate(feePayer: .you), force: false))
        XCTAssertFalse(lspPaysCloseFee(nil, force: false))
    }

    func testNonAnchorWarningOnlyForForceWithKnownNonAnchor() {
        XCTAssertTrue(showsNonAnchorWarning(closeEstimate(isAnchor: false), force: true))
        XCTAssertFalse(showsNonAnchorWarning(closeEstimate(isAnchor: false), force: false))
        XCTAssertFalse(showsNonAnchorWarning(closeEstimate(isAnchor: true), force: true))
        XCTAssertFalse(showsNonAnchorWarning(closeEstimate(isAnchor: nil), force: true))
        XCTAssertFalse(showsNonAnchorWarning(nil, force: true))
    }

    func testInFlightWarningPluralizesLikeThePwa() {
        XCTAssertNil(pendingHtlcWarning(closeEstimate(pendingHtlcCount: 0)))
        XCTAssertNil(pendingHtlcWarning(nil))
        XCTAssertEqual(
            "1 in-flight payment must settle before the close completes — "
                + "the amount returned may change.",
            pendingHtlcWarning(closeEstimate(pendingHtlcCount: 1))
        )
        XCTAssertEqual(
            "3 in-flight payments must settle before the close completes — "
                + "the amount returned may change.",
            pendingHtlcWarning(closeEstimate(pendingHtlcCount: 3))
        )
    }

    // MARK: CTA + success copy variants

    func testCtaLabelFollowsTheCloseMethod() {
        XCTAssertEqual("Close Channel", closeCtaLabel(force: false, closing: false))
        XCTAssertEqual("Force Close Channel", closeCtaLabel(force: true, closing: false))
        XCTAssertEqual("Closing…", closeCtaLabel(force: false, closing: true))
    }

    func testSuccessDetailVariesByMethodAndTimelock() {
        XCTAssertEqual(
            "Your channel is closing. Funds return to your wallet once the closing "
                + "transaction confirms on-chain — keep the app open until the close completes.",
            closeSuccessDetail(force: false, estimate: nil)
        )
        XCTAssertEqual(
            "Force close initiated. Your funds will be accessible in ~24 hours — they "
                + "return to your wallet automatically once the timelock expires.",
            closeSuccessDetail(force: true, estimate: closeEstimate(timelockBlocks: 144))
        )
        XCTAssertEqual(
            "Force close initiated. Your funds will be accessible in ~14 days — they "
                + "return to your wallet automatically once the timelock expires.",
            closeSuccessDetail(force: true, estimate: nil)
        )
    }

    // MARK: failure mapping + escalation offer

    func testCoopFailureOffersForceCloseEscalation() {
        let failure = closeFailure(
            WalletException.ChannelCloseFailed(detail: "peer offline"), force: false
        )
        XCTAssertEqual("Close failed: peer offline", failure.message)
        XCTAssertTrue(failure.canForceClose)
    }

    func testForceFailureDoesNotEscalateFurther() {
        let failure = closeFailure(KotlinRuntimeException(message: nil), force: true)
        XCTAssertEqual("Force close failed.", failure.message)
        XCTAssertFalse(failure.canForceClose)
    }

    func testUnknownCoopFailureUsesThePwaDefaultCopy() {
        let failure = closeFailure(KotlinRuntimeException(message: nil), force: false)
        XCTAssertEqual(
            "Cooperative close failed. The peer may be disconnected or the channel "
                + "has pending payments.",
            failure.message
        )
        XCTAssertTrue(failure.canForceClose)
    }
}
