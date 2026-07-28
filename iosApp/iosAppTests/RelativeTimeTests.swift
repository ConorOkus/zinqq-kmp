import XCTest

@testable import iosApp

/// Vectors for the Activity list's relative-time buckets, mirroring the PWA's
/// `formatRelativeTime` (`Activity.tsx:15-27`) and Android's
/// `RelativeTimeTest`: Just now under a minute, then floor-divided m/h/d/w
/// buckets, and empty for the zero sentinel timestamp.
final class RelativeTimeTests: XCTestCase {
    private let now: Int64 = 1_753_500_000_000

    private func at(_ secondsAgo: Int64) -> String {
        formatRelativeTime(timestampMs: now - secondsAgo * 1_000, nowMs: now)
    }

    func testZeroTimestampRendersEmpty() {
        XCTAssertEqual("", formatRelativeTime(timestampMs: 0, nowMs: now))
    }

    func testUnderAMinuteIsJustNow() {
        XCTAssertEqual("Just now", at(0))
        XCTAssertEqual("Just now", at(5))
        XCTAssertEqual("Just now", at(59))
    }

    func testMinuteBucketUpToAnHour() {
        XCTAssertEqual("1m ago", at(60))
        XCTAssertEqual("5m ago", at(5 * 60))
        XCTAssertEqual("59m ago", at(59 * 60 + 59))
    }

    func testHourBucketUpToADay() {
        XCTAssertEqual("1h ago", at(60 * 60))
        XCTAssertEqual("3h ago", at(3 * 60 * 60))
        XCTAssertEqual("23h ago", at(23 * 60 * 60 + 59 * 60))
    }

    func testDayBucketUpToAWeek() {
        XCTAssertEqual("1d ago", at(24 * 60 * 60))
        XCTAssertEqual("2d ago", at(2 * 24 * 60 * 60))
        XCTAssertEqual("6d ago", at(6 * 24 * 60 * 60 + 23 * 60 * 60))
    }

    func testWeekBucketIsOpenEnded() {
        XCTAssertEqual("1w ago", at(7 * 24 * 60 * 60))
        XCTAssertEqual("3w ago", at(3 * 7 * 24 * 60 * 60))
        XCTAssertEqual("52w ago", at(364 * 24 * 60 * 60))
    }
}
