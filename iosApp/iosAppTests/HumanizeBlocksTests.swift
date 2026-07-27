import XCTest

@testable import iosApp

/// Vectors for the close-detail countdown's block humanizer, mirroring the
/// PWA's `humanizeBlocks` (`close-records/estimate.ts:60-66`) and Android's
/// `HumanizeBlocksTest`: 10 minutes a block, minutes under an hour, rounded
/// hours under 48, rounded days after.
final class HumanizeBlocksTests: XCTestCase {
    func testUnderAnHourRendersMinutes() {
        XCTAssertEqual("~10 minutes", humanizeBlocks(1))
        XCTAssertEqual("~50 minutes", humanizeBlocks(5))
    }

    func testUnderTwoDaysRendersRoundedHours() {
        XCTAssertEqual("~1 hour", humanizeBlocks(6))
        XCTAssertEqual("~2 hours", humanizeBlocks(12))
        // 144 blocks is a day of blocks but still under the 48h switch.
        XCTAssertEqual("~24 hours", humanizeBlocks(144))
        XCTAssertEqual("~47 hours", humanizeBlocks(283))
    }

    func testTwoDaysAndUpRendersRoundedDays() {
        XCTAssertEqual("~2 days", humanizeBlocks(288))
        // The canonical force-close timelock: 2016 blocks ≈ 14 days.
        XCTAssertEqual("~14 days", humanizeBlocks(2016))
    }
}
