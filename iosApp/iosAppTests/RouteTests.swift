import XCTest

@testable import iosApp

/// The 16-route contract (U18, R12), mirroring Android's `RoutesTest`: paths
/// mirror the PWA's `router.tsx` and each screen's declared `backTo` matches
/// the PWA's header links — AppShell's destination-based navigation walks
/// this table, never history pops.
final class RouteTests: XCTestCase {
    func testAllSixteenPwaRoutesExist() {
        XCTAssertEqual(16, Route.all.count)
        XCTAssertEqual(
            [
                "home",
                "receive",
                "send",
                "scan",
                "activity",
                "activity/close/{channelId}",
                "activity/{txId}",
                "recover",
                "settings",
                "settings/backup",
                "settings/restore",
                "settings/advanced",
                "settings/advanced/balance",
                "settings/advanced/peers",
                "settings/advanced/peers/open-channel",
                "settings/advanced/peers/close-channel",
            ],
            Route.all.map(\.pattern)
        )
    }

    func testBackToTargetsMirrorThePwaHeaders() {
        XCTAssertNil(Route.home.backTo)
        XCTAssertNil(Route.activity.backTo)
        XCTAssertEqual(.home, Route.receive.backTo)
        XCTAssertEqual(.home, Route.send.backTo)
        XCTAssertEqual(.home, Route.scan.backTo)
        XCTAssertEqual(.home, Route.recover.backTo)
        XCTAssertEqual(.home, Route.settings.backTo)
        XCTAssertEqual(.activity, Route.activityCloseDetail(channelId: "abc").backTo)
        XCTAssertEqual(.activity, Route.activityTxDetail(txId: "abc").backTo)
        XCTAssertEqual(.settings, Route.settingsBackup.backTo)
        XCTAssertEqual(.settings, Route.settingsRestore.backTo)
        XCTAssertEqual(.settings, Route.settingsAdvanced.backTo)
        XCTAssertEqual(.settingsAdvanced, Route.advancedBalance.backTo)
        XCTAssertEqual(.settingsAdvanced, Route.advancedPeers.backTo)
        XCTAssertEqual(.advancedPeers, Route.peersOpenChannel.backTo)
        XCTAssertEqual(.advancedPeers, Route.peersCloseChannel.backTo)
    }

    func testTabBarShowsOnlyOnHomeAndActivity() {
        XCTAssertTrue(Route.home.showsTabBar)
        XCTAssertTrue(Route.activity.showsTabBar)
        for route in Route.all where route != .home && route != .activity {
            XCTAssertFalse(route.showsTabBar, "\(route) must not show the tab bar")
        }
    }

    func testArgumentRoutesBuildConcretePaths() {
        XCTAssertEqual("activity/close/abc123", Route.activityCloseDetail(channelId: "abc123").path)
        XCTAssertEqual("activity/deadbeef", Route.activityTxDetail(txId: "deadbeef").path)
        XCTAssertEqual("settings/backup", Route.settingsBackup.path)
    }

    /// The NavigationStack path for a destination is its backTo chain, so a
    /// swipe-back pop always lands on the declared backTo target (KTD-11).
    func testBackChainWalksTheDeclaredBackToTargets() {
        XCTAssertEqual([], Route.home.backChain)
        XCTAssertEqual([.activity], Route.activity.backChain)
        XCTAssertEqual([.receive], Route.receive.backChain)
        XCTAssertEqual([.settings, .settingsBackup], Route.settingsBackup.backChain)
        XCTAssertEqual(
            [.settings, .settingsAdvanced, .advancedPeers, .peersOpenChannel],
            Route.peersOpenChannel.backChain
        )
        XCTAssertEqual(
            [.activity, .activityTxDetail(txId: "tx1")],
            Route.activityTxDetail(txId: "tx1").backChain
        )
    }
}
