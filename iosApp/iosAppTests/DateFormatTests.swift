import XCTest

@testable import iosApp

/// en-GB date/time stability, pinned to what the PWA's
/// `Intl.DateTimeFormat('en-GB', …)` produces (verified against V8) and
/// byte-identical to Android's `DateFormatTest`:
/// - TransactionDetail (`TransactionDetail.tsx:9-27`): "Sun, 26 July 2026" /
///   "14:05:09", both "Pending" for the zero sentinel.
/// - ChannelCloseDetail (`ChannelCloseDetail.tsx:32-40`):
///   "26 July 2026 at 14:05".
/// Tests pass UTC explicitly; screens use the device zone like the PWA.
final class DateFormatTests: XCTestCase {
    // 2026-07-26T14:05:09Z
    private let ts: Int64 = 1_785_074_709_000
    private let utc = TimeZone(identifier: "UTC")!

    func testTransactionDetailDate() {
        XCTAssertEqual("Sun, 26 July 2026", formatDetailDate(timestampMs: ts, timeZone: utc))
    }

    func testTransactionDetailTimeIs24HourWithSeconds() {
        XCTAssertEqual("14:05:09", formatDetailTime(timestampMs: ts, timeZone: utc))
    }

    func testZeroTimestampRendersPending() {
        XCTAssertEqual("Pending", formatDetailDate(timestampMs: 0, timeZone: utc))
        XCTAssertEqual("Pending", formatDetailTime(timestampMs: 0, timeZone: utc))
    }

    func testCloseDetailDateIncludesTheTime() {
        XCTAssertEqual("26 July 2026 at 14:05", formatCloseDate(timestampMs: ts, timeZone: utc))
    }

    func testCloseDetailDateZeroPadsAndDropsLeadingDayZero() {
        // 2026-01-05T09:03:00Z — single-digit day stays bare, time zero-pads.
        XCTAssertEqual(
            "5 January 2026 at 09:03",
            formatCloseDate(timestampMs: 1_767_603_780_000, timeZone: utc)
        )
    }
}
