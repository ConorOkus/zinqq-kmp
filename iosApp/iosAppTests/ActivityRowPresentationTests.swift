import Shared
import XCTest

@testable import iosApp

/// Row-derivation matrix for the Activity list, mirroring the PWA's
/// `Activity.tsx` and Android's `ActivityRowPresentationTest`: titles,
/// Pending/close-status badges (`CLOSE_BADGES`, lines 7-13), signed
/// `formatBtc` amounts with msat floored for Lightning rows, the em-dash for
/// unknown close amounts, muted styling while pending, and the `⚡` glyph on
/// Lightning and close rows.
final class ActivityRowPresentationTests: XCTestCase {
    func testTitlesFollowDirectionAndKind() {
        XCTAssertEqual("Sent", activityTitle(activityRow(direction: .sent)))
        XCTAssertEqual("Received", activityTitle(activityRow(direction: .received)))
        XCTAssertEqual(
            "Channel close",
            activityTitle(activityRow(kind: .channelClose, direction: nil))
        )
    }

    func testBadgeIsPendingForNonCloseRows() {
        XCTAssertEqual("Pending", activityBadge(activityRow(status: .pending)))
        XCTAssertNil(activityBadge(activityRow(status: .confirmed)))
        XCTAssertEqual(
            "Pending",
            activityBadge(activityRow(kind: .onchain, status: .pending))
        )
    }

    func testBadgeMapsCloseStatusesLikeThePwaCloseBadgesTable() {
        func closeRow(_ status: CloseStatusLabel) -> ActivityRow {
            activityRow(kind: .channelClose, status: .pending, closeStatus: status)
        }
        XCTAssertEqual("Closing", activityBadge(closeRow(.closing)))
        XCTAssertEqual("Waiting timelock", activityBadge(closeRow(.waitingTimelock)))
        XCTAssertEqual("Returning to wallet", activityBadge(closeRow(.returning)))
        XCTAssertNil(activityBadge(closeRow(.complete)))
        XCTAssertEqual("Resolved", activityBadge(closeRow(.resolvedUnverified)))
    }

    func testLightningAmountsFloorMsatAndSignByDirection() {
        XCTAssertEqual(
            "-₿250",
            activityAmountText(activityRow(direction: .sent, amountMsat: 250_999))
        )
        XCTAssertEqual(
            "+₿1,234",
            activityAmountText(activityRow(direction: .received, amountMsat: 1_234_000))
        )
    }

    func testOnchainAmountsUseSatsDirectly() {
        XCTAssertEqual(
            "+₿2,000",
            activityAmountText(
                activityRow(kind: .onchain, direction: .received, amountSats: 2_000)
            )
        )
    }

    func testCloseRowsRenderPlusAmountOrEmDashWhenUnknown() {
        XCTAssertEqual(
            "+₿5,000",
            activityAmountText(
                activityRow(kind: .channelClose, direction: nil, amountSats: 5_000)
            )
        )
        XCTAssertEqual(
            "—",
            activityAmountText(
                activityRow(kind: .channelClose, direction: nil, amountSats: nil)
            )
        )
    }

    func testPendingRowsAreMuted() {
        XCTAssertTrue(isAmountMuted(activityRow(status: .pending)))
        XCTAssertFalse(isAmountMuted(activityRow(status: .confirmed)))
    }

    func testLightningGlyphShowsOnLightningAndCloseRowsOnly() {
        XCTAssertTrue(showsLightningGlyph(activityRow(kind: .lightning)))
        XCTAssertTrue(showsLightningGlyph(activityRow(kind: .channelClose)))
        XCTAssertFalse(showsLightningGlyph(activityRow(kind: .onchain)))
    }
}
