import XCTest

@testable import iosApp

/// The event set that re-queries wallet data (balances, activity, recovery,
/// pending sweep), mirroring Android's `RefreshTriggerTest`: the spike's
/// balance triggers extended with the sweep and recovery change events (U19;
/// the PWA's `usePendingSweep`/`useRecovery` re-read on their change
/// notifications).
final class RefreshTriggerTests: XCTestCase {
    func testSettlementAndStateChangeEventsTriggerARefresh() {
        XCTAssertTrue(
            shouldRefreshWalletData(
                .paymentReceived(paymentHash: "hash", amountMsat: 1_000, skimmedFeeMsat: nil)
            )
        )
        XCTAssertTrue(shouldRefreshWalletData(.paymentSuccessful(paymentHash: "hash")))
        XCTAssertTrue(shouldRefreshWalletData(.channelReady))
        XCTAssertTrue(shouldRefreshWalletData(.sweepStateChanged))
        XCTAssertTrue(shouldRefreshWalletData(.recoveryStateChanged))
    }

    func testOtherEventsDoNot() {
        XCTAssertFalse(shouldRefreshWalletData(.nodeStarted))
        XCTAssertFalse(shouldRefreshWalletData(.nodeStopped))
        XCTAssertFalse(shouldRefreshWalletData(.syncCompleted))
        XCTAssertFalse(shouldRefreshWalletData(.syncFailed))
        XCTAssertFalse(
            shouldRefreshWalletData(.invoiceReady(bolt11: "lnbc1", expiryUnixSecs: 0))
        )
        XCTAssertFalse(shouldRefreshWalletData(.channelPending))
        XCTAssertFalse(shouldRefreshWalletData(.unknown))
    }
}
