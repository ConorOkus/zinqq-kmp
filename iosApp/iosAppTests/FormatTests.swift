import Shared
import XCTest

/// BIP177 / msat formatting consumed straight from the shared KMP framework
/// (U18, KTD-11, R12): the same vectors as the PWA's `format-btc.test.ts` /
/// `msat.test.ts` and Android's `FormatTest`, proving both shells render
/// identical amounts from the same commonMain code.
final class FormatTests: XCTestCase {
    func testFormatsZero() {
        XCTAssertEqual("₿0", FormatKt.formatBtc(sats: 0))
    }

    func testFormatsSmallAmountsWithoutCommas() {
        XCTAssertEqual("₿1", FormatKt.formatBtc(sats: 1))
        XCTAssertEqual("₿999", FormatKt.formatBtc(sats: 999))
    }

    func testFormatsAmountsWithCommaSeparation() {
        XCTAssertEqual("₿1,000", FormatKt.formatBtc(sats: 1_000))
        XCTAssertEqual("₿50,000", FormatKt.formatBtc(sats: 50_000))
        XCTAssertEqual("₿1,234,567", FormatKt.formatBtc(sats: 1_234_567))
        XCTAssertEqual("₿100,000,000", FormatKt.formatBtc(sats: 100_000_000))
    }

    func testHandlesLargeValues() {
        XCTAssertEqual("₿2,100,000,000,000,000", FormatKt.formatBtc(sats: 2_100_000_000_000_000))
    }

    func testHandlesNegativeAmounts() {
        XCTAssertEqual("-₿50,000", FormatKt.formatBtc(sats: -50_000))
        XCTAssertEqual("-₿1", FormatKt.formatBtc(sats: -1))
    }

    func testMsatToSatFloor() {
        XCTAssertEqual(5, FormatKt.msatToSatFloor(msat: 5_000))
        XCTAssertEqual(1, FormatKt.msatToSatFloor(msat: 1_999))
        XCTAssertEqual(1, FormatKt.msatToSatFloor(msat: 1_001))
        XCTAssertEqual(0, FormatKt.msatToSatFloor(msat: 999))
        XCTAssertEqual(100_000_000, FormatKt.msatToSatFloor(msat: 100_000_000_999))
    }

    func testMsatToSatCeil() {
        XCTAssertEqual(5, FormatKt.msatToSatCeil(msat: 5_000))
        XCTAssertEqual(2, FormatKt.msatToSatCeil(msat: 1_999))
        XCTAssertEqual(2, FormatKt.msatToSatCeil(msat: 1_001))
        XCTAssertEqual(1, FormatKt.msatToSatCeil(msat: 999))
        XCTAssertEqual(1, FormatKt.msatToSatCeil(msat: 1))
        XCTAssertEqual(100_000_001, FormatKt.msatToSatCeil(msat: 100_000_000_001))
    }

    func testSatsToBtcStringUsesEightDecimalPlaces() {
        XCTAssertEqual("0.00000000", FormatKt.satsToBtcString(sats: 0))
        XCTAssertEqual("0.00050000", FormatKt.satsToBtcString(sats: 50_000))
        XCTAssertEqual("1.00000000", FormatKt.satsToBtcString(sats: 100_000_000))
        XCTAssertEqual("1.23456789", FormatKt.satsToBtcString(sats: 123_456_789))
        XCTAssertEqual("21000000.00000000", FormatKt.satsToBtcString(sats: 2_100_000_000_000_000))
        XCTAssertEqual("-0.00000001", FormatKt.satsToBtcString(sats: -1))
    }
}
