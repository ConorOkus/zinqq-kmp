import Shared
import XCTest

@testable import iosApp

/// Settings/Advanced/Balance pure derivations (U22, R12): the PWA's row
/// tables with How-It-Works/Get-Help preserved as inert no-ops
/// (`Settings.tsx:12-107`, `Advanced.tsx:6-47`), the appearance radiogroup
/// mapping (`Settings.tsx:6-10`, `theme.ts:3`), and the Balance breakdown
/// (`Balance.tsx`, `use-unified-balance.ts`) — Android's `SettingsLogicTest`
/// ported fixture-for-fixture.
final class SettingsLogicTests: XCTestCase {

    // MARK: appearance radiogroup (theme.ts:3 order, Settings.tsx labels)

    func testAppearanceModesKeepThePwaOrder() {
        XCTAssertEqual([AppearanceMode.hybrid, .light, .dark], appearanceModes)
    }

    func testAppearanceLabelsMatchThePwa() {
        XCTAssertEqual("Hybrid", appearanceLabel(.hybrid))
        XCTAssertEqual("Light", appearanceLabel(.light))
        XCTAssertEqual("Dark", appearanceLabel(.dark))
    }

    // MARK: rows (Settings.tsx / Advanced.tsx tables)

    func testSettingsRowsMirrorThePwaTableWithInertHelpRows() {
        XCTAssertEqual(
            [
                "Wallet Backup", "Recover Wallet", "Advanced", "How It Works", "Get Help",
            ],
            settingsRows.map(\.label)
        )
        XCTAssertEqual(
            ["Setup", "From Seed", "Settings", "FAQ", "Chat with us"],
            settingsRows.map(\.detail)
        )
        XCTAssertEqual(Route.settingsBackup, settingsRows[0].destination)
        XCTAssertEqual(Route.settingsRestore, settingsRows[1].destination)
        XCTAssertEqual(Route.settingsAdvanced, settingsRows[2].destination)
        // The PWA ships these as no-ops (route: null) — replicated inert.
        XCTAssertNil(settingsRows[3].destination)
        XCTAssertNil(settingsRows[4].destination)
    }

    func testAdvancedRowsMirrorThePwaTable() {
        XCTAssertEqual(["Balance", "Peers"], advancedRows.map(\.label))
        XCTAssertEqual(["Onchain · Lightning", "Connected"], advancedRows.map(\.detail))
        XCTAssertEqual(Route.advancedBalance, advancedRows[0].destination)
        XCTAssertEqual(Route.advancedPeers, advancedRows[1].destination)
    }

    // MARK: Balance breakdown (use-unified-balance.ts)

    func testBreakdownSplitsOnchainAndFlooredLightning() {
        let breakdown = balanceBreakdown(
            balancesFixture(
                lightningMsat: 2_500_999,
                onchainTotalSats: 10_000,
                onchainSpendableSats: 8_000,
                onchainUntrustedPendingSats: 2_000
            )
        )
        XCTAssertEqual(10_000, breakdown.onchainSats)
        XCTAssertEqual(2_500, breakdown.lightningSats)
        XCTAssertEqual(12_500, breakdown.totalSats)
        XCTAssertEqual(2_000, breakdown.pendingSats)
    }

    func testPendingLineShowsOnlyWhenPositive() {
        XCTAssertEqual(0, balanceBreakdown(balancesFixture()).pendingSats)
    }
}
