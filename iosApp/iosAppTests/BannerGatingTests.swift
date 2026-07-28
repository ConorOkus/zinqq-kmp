import XCTest

@testable import iosApp

/// Home banner gating (U19, R9), mirroring Android's `BannerGatingTest`:
/// - RecoveryBanner renders whenever recovery state exists (`Home.tsx:80-84`),
///   with the PWA's two variants (`RecoveryBanner.tsx`); only the
///   sweep-confirmed variant is dismissible.
/// - PendingSweepBanner is `lastAttemptFailed`-gated (`Home.tsx:86-90`) with
///   the heading/subtext/deep-link matrix from `PendingSweepBanner.tsx`.
final class BannerGatingTests: XCTestCase {
    // MARK: RecoveryBanner

    func testNoRecoveryStateShowsNoBanner() {
        XCTAssertNil(recoveryBanner(nil, dismissed: false))
    }

    func testNeedsRecoveryBannerNavigatesToRecover() throws {
        let banner = try XCTUnwrap(
            recoveryBanner(recoveryStateView(status: .needsRecovery), dismissed: false)
        )
        XCTAssertEqual("Your funds are safe", banner.title)
        XCTAssertEqual("A small deposit is needed to unlock them", banner.subtitle)
        XCTAssertTrue(banner.navigatesToRecover)
        XCTAssertFalse(banner.dismissible)
    }

    func testSweepConfirmedBannerIsDismissible() throws {
        let banner = try XCTUnwrap(
            recoveryBanner(recoveryStateView(status: .sweepConfirmed), dismissed: false)
        )
        XCTAssertEqual("Funds recovered!", banner.title)
        XCTAssertEqual("Available in approximately 14 days", banner.subtitle)
        XCTAssertFalse(banner.navigatesToRecover)
        XCTAssertTrue(banner.dismissible)
    }

    func testDismissalHidesOnlyTheSweepConfirmedVariant() {
        XCTAssertNil(
            recoveryBanner(recoveryStateView(status: .sweepConfirmed), dismissed: true)
        )
        // Needs-recovery has no dismiss affordance; a stale flag never hides it.
        XCTAssertEqual(
            "Your funds are safe",
            recoveryBanner(recoveryStateView(status: .needsRecovery), dismissed: true)?.title
        )
    }

    @MainActor
    func testRecoveryStateChangeResetsTheDismissal() {
        let model = WalletModel()
        model.dismissRecoveryBanner()
        XCTAssertTrue(model.recoveryBannerDismissed)
        model.handle(.recoveryStateChanged)
        XCTAssertFalse(model.recoveryBannerDismissed)
    }

    // MARK: PendingSweepBanner

    func testSweepBannerOnlyShowsAfterAFailedAttempt() {
        XCTAssertNil(sweepBanner(nil))
        XCTAssertNil(sweepBanner(pendingSweepView(lastAttemptFailed: false)))
    }

    func testHeadingShowsTheAmountWaitingToSweep() throws {
        let banner = try XCTUnwrap(sweepBanner(pendingSweepView(pendingSats: 5_000)))
        XCTAssertEqual("₿5,000 waiting to sweep", banner.heading)
    }

    func testUnknownValueUndercountsGetAnAtLeastPrefix() throws {
        let banner = try XCTUnwrap(
            sweepBanner(pendingSweepView(pendingSats: 5_000, hasUnknownValue: true))
        )
        XCTAssertEqual("At least ₿5,000 waiting to sweep", banner.heading)
    }

    func testZeroPendingFallsBackToGenericHeading() throws {
        let banner = try XCTUnwrap(sweepBanner(pendingSweepView(pendingSats: 0)))
        XCTAssertEqual("Funds waiting to sweep", banner.heading)
    }

    func testNeedsFundsWithShortfallAsksForAtLeastThatAmountAndLinksToReceive() throws {
        let banner = try XCTUnwrap(
            sweepBanner(pendingSweepView(needsOnchainFunds: true, shortfallSats: 800))
        )
        XCTAssertEqual(
            "Add at least ₿800 to cover network fees and recover these funds",
            banner.subtitle
        )
        XCTAssertTrue(banner.navigatesToReceive)
    }

    func testNeedsFundsWithoutShortfallUsesTheGenericAsk() throws {
        let banner = try XCTUnwrap(
            sweepBanner(pendingSweepView(needsOnchainFunds: true, shortfallSats: nil))
        )
        XCTAssertEqual(
            "Add bitcoin to cover network fees and recover these funds",
            banner.subtitle
        )
        XCTAssertTrue(banner.navigatesToReceive)
    }

    func testSelfSufficientSweepIsInformationalOnly() throws {
        let banner = try XCTUnwrap(sweepBanner(pendingSweepView(needsOnchainFunds: false)))
        XCTAssertEqual(
            "Recovered funds return to your balance automatically when network fees allow",
            banner.subtitle
        )
        XCTAssertFalse(banner.navigatesToReceive)
    }
}
