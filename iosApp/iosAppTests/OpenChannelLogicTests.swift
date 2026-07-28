import Shared
import XCTest

@testable import iosApp

/// OpenChannel's pure derivations (U22, R10 UI): the PWA's 20,000–16,777,215
/// bounds and balance gate (`OpenChannel.tsx:29-31,83-111`), the review fee /
/// total math (`OpenChannel.tsx:97-98,262-273`), and the typed-error copy —
/// Android's `OpenChannelLogicTest` ported fixture-for-fixture.
final class OpenChannelLogicTests: XCTestCase {

    private let fee = OpenFeeEstimate(feeRateSatPerVb: 3, estimatedFeeSats: 420)

    // MARK: bounds matrix (PWA copy verbatim)

    func testBelowMinimumIsRejected() {
        XCTAssertEqual(
            "Minimum channel size is ₿20,000",
            validateOpenAmount(
                amountSats: 19_999,
                estimatedFeeSats: fee.estimatedFeeSats,
                balanceSats: 1_000_000
            )
        )
    }

    func testBoundsAreInclusive() {
        XCTAssertNil(
            validateOpenAmount(
                amountSats: 20_000,
                estimatedFeeSats: fee.estimatedFeeSats,
                balanceSats: 1_000_000
            )
        )
        XCTAssertNil(
            validateOpenAmount(
                amountSats: 16_777_215, estimatedFeeSats: 0, balanceSats: 20_000_000
            )
        )
    }

    func testAboveMaximumIsRejected() {
        XCTAssertEqual(
            "Maximum channel size is ₿16,777,215",
            validateOpenAmount(
                amountSats: 16_777_216, estimatedFeeSats: 0, balanceSats: 20_000_000
            )
        )
    }

    func testAmountPlusFeeMustFitTheBalance() {
        // 20,000 + 420 > 20,419 → rejected; = 20,420 → allowed.
        XCTAssertEqual(
            "Amount plus fees exceeds available balance",
            validateOpenAmount(amountSats: 20_000, estimatedFeeSats: 420, balanceSats: 20_419)
        )
        XCTAssertNil(
            validateOpenAmount(amountSats: 20_000, estimatedFeeSats: 420, balanceSats: 20_420)
        )
    }

    // MARK: review derivations

    func testFeeRowLabelsTheRateAndTotalAddsTheFee() {
        XCTAssertEqual("Est. fee (~3 sat/vB)", openFeeRateLabel(fee.feeRateSatPerVb))
        XCTAssertEqual(
            20_420, openTotalSats(amountSats: 20_000, estimatedFeeSats: fee.estimatedFeeSats)
        )
    }

    func testEstimateFallbackMirrorsThePwaOneSatPerVb() {
        // PWA `getFeeRate` failure → 1 sat/vB × 140 vB
        // (`OpenChannel.tsx:70-72,97-98`).
        let fallback = fallbackOpenFee()
        XCTAssertEqual(1, fallback.feeRateSatPerVb)
        XCTAssertEqual(140, fallback.estimatedFeeSats)
    }

    func testReviewPeerIsMidTruncated() {
        XCTAssertEqual(
            String(peerPubkey.prefix(12)) + "..." + String(peerPubkey.suffix(8)),
            reviewPeerDisplay(peerPubkey)
        )
    }

    // MARK: typed-error copy

    func testTypedOpenErrorsCarryTheCoreParityCopy() {
        XCTAssertEqual(
            "Minimum channel size is ₿20,000",
            openChannelErrorMessage(WalletException.ChannelAmountBelowMinimum())
        )
        XCTAssertEqual(
            "Maximum channel size is ₿16,777,215",
            openChannelErrorMessage(WalletException.ChannelAmountAboveMaximum())
        )
        XCTAssertEqual(
            "Amount plus fees exceeds available balance",
            openChannelErrorMessage(WalletException.ChannelAmountExceedsBalance())
        )
        XCTAssertEqual(
            "Failed to connect to peer: dial timed out",
            openChannelErrorMessage(WalletException.PeerConnectFailed(detail: "dial timed out"))
        )
        XCTAssertEqual(
            "Failed to initiate channel opening: rejected",
            openChannelErrorMessage(WalletException.ChannelOpenFailed(detail: "rejected"))
        )
        XCTAssertEqual(
            "Invalid peer address: expected pubkey@host:port",
            openChannelErrorMessage(
                WalletException.InvalidPeerAddress(
                    detail: "Invalid peer address: expected pubkey@host:port"
                )
            )
        )
    }

    func testUnknownOpenFailuresFallBackToThePwaGenericCopy() {
        XCTAssertEqual(
            "Failed to initiate channel opening. The peer may have disconnected.",
            openChannelErrorMessage(KotlinRuntimeException(message: nil))
        )
    }
}
