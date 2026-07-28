import XCTest

@testable import iosApp

/// The PWA's numpad reducer semantics (U18, R12): 8-digit cap, leading-zero
/// collapse, backspace — the SAME fixtures as Android's `NumpadReducerTest`,
/// but consumed through the shared KMP framework via the app's `NumpadReducer`
/// adapter, proving reducer parity across the FFI rather than a Swift port.
final class NumpadReducerTests: XCTestCase {
    func testAppendsDigits() {
        XCTAssertEqual("5", NumpadReducer.reduce("", .digit("5")))
        XCTAssertEqual("50", NumpadReducer.reduce("5", .digit("0")))
        XCTAssertEqual("509", NumpadReducer.reduce("50", .digit("9")))
    }

    func testBackspaceDropsTheLastDigit() {
        XCTAssertEqual("5", NumpadReducer.reduce("50", .backspace))
        XCTAssertEqual("", NumpadReducer.reduce("5", .backspace))
    }

    func testBackspaceOnEmptyStaysEmpty() {
        XCTAssertEqual("", NumpadReducer.reduce("", .backspace))
    }

    func testCapsAtEightDigits() {
        XCTAssertEqual(8, NumpadReducer.maxDigits)
        XCTAssertEqual("12345678", NumpadReducer.reduce("12345678", .digit("9")))
    }

    func testCapIsConfigurable() {
        XCTAssertEqual("123", NumpadReducer.reduce("123", .digit("4"), maxDigits: 3))
        XCTAssertEqual("1234", NumpadReducer.reduce("123", .digit("4"), maxDigits: 4))
    }

    func testBackspaceStillWorksAtTheCap() {
        XCTAssertEqual("1234567", NumpadReducer.reduce("12345678", .backspace))
    }

    func testLeadingZeroNeverAccumulates() {
        XCTAssertEqual("0", NumpadReducer.reduce("", .digit("0")))
        XCTAssertEqual("0", NumpadReducer.reduce("0", .digit("0")))
    }

    func testLeadingZeroCollapsesToTheNextDigit() {
        XCTAssertEqual("5", NumpadReducer.reduce("0", .digit("5")))
    }
}
