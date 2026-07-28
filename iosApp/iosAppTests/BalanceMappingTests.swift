import XCTest

@testable import iosApp

/// Home balance mapping, mirroring the PWA's `useUnifiedBalance`
/// (`use-unified-balance.ts`) and Android's `BalanceMappingTest`: total =
/// full on-chain (confirmed + all pending) + floored Lightning sats; the
/// "+₿X pending" line is exactly the untrusted pending (unconfirmed external
/// receives).
final class BalanceMappingTests: XCTestCase {
    func testTotalIsOnchainTotalPlusFlooredLightning() {
        let balance = homeBalance(
            balancesFixture(
                lightningMsat: 2_500_999,
                onchainTotalSats: 10_000,
                onchainSpendableSats: 8_000,
                onchainUntrustedPendingSats: 2_000
            )
        )
        XCTAssertEqual(12_500, balance.totalSats)
    }

    func testPendingLineIsUntrustedPendingOnly() {
        let balance = homeBalance(
            balancesFixture(
                lightningMsat: 1_000_000,
                onchainTotalSats: 10_000,
                onchainSpendableSats: 8_000,
                onchainUntrustedPendingSats: 2_000
            )
        )
        XCTAssertEqual(2_000, balance.pendingSats)
    }

    func testZeroBalancesMapToZero() {
        let balance = homeBalance(balancesFixture())
        XCTAssertEqual(0, balance.totalSats)
        XCTAssertEqual(0, balance.pendingSats)
    }
}
