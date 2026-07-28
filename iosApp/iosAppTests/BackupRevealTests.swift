import Shared
import XCTest

@testable import iosApp

/// The Backup reveal machine (U22, R1 UI): the PWA's 60-second auto-hide with
/// a live "Hides in Ns" countdown, plus the visibilitychange → scenePhase
/// hide (`Backup.tsx:8,46-65,127-129`) and the reveal-failure copy
/// (`Backup.tsx:28-43`) — Android's `BackupRevealTest` ported
/// fixture-for-fixture.
final class BackupRevealTests: XCTestCase {

    func testRevealSplitsTheMnemonicAndStartsAt60() {
        let ui = revealBackup(
            "abandon ability able about above absent absorb abstract absurd abuse access accident"
        )
        guard case let .revealed(words, secondsLeft) = ui else {
            return XCTFail("expected revealed, got \(ui)")
        }
        XCTAssertEqual(12, words.count)
        XCTAssertEqual("abandon", words.first)
        XCTAssertEqual("accident", words.last)
        XCTAssertEqual(backupAutoHideSecs, secondsLeft)
    }

    func testRevealToleratesExtraWhitespace() {
        let ui = revealBackup("  one   two\nthree ")
        guard case let .revealed(words, _) = ui else {
            return XCTFail("expected revealed, got \(ui)")
        }
        XCTAssertEqual(["one", "two", "three"], words)
    }

    func testTickCountsDownOneSecondAtATime() {
        var ui = revealBackup("a b c")
        ui = tickBackup(ui)
        guard case let .revealed(_, secondsLeft) = ui else {
            return XCTFail("expected revealed, got \(ui)")
        }
        XCTAssertEqual(59, secondsLeft)
        XCTAssertEqual("Hides in 59s", backupCountdownText(secondsLeft: secondsLeft))
    }

    func testTickAtZeroAutoHides() {
        var ui = BackupUi.revealed(words: ["a"], secondsLeft: 1)
        ui = tickBackup(ui)
        XCTAssertEqual(BackupUi.warning, ui)
    }

    func testTickOnNonRevealedStatesIsIdentity() {
        XCTAssertEqual(BackupUi.warning, tickBackup(.warning))
        let error = BackupUi.error(message: "boom")
        XCTAssertEqual(error, tickBackup(error))
    }

    func testLifecycleHideOnlyCollapsesTheRevealedGrid() {
        // scenePhase leaving .active while revealed → back to the warning
        // (the PWA's immediate visibilitychange hide).
        XCTAssertEqual(
            BackupUi.warning, hideBackup(.revealed(words: ["a"], secondsLeft: 42))
        )
        // Warning and error states are untouched by backgrounding.
        XCTAssertEqual(BackupUi.warning, hideBackup(.warning))
        let error = BackupUi.error(message: "boom")
        XCTAssertEqual(error, hideBackup(error))
    }

    func testReRevealRestartsTheCountdown() {
        var ui = revealBackup("a b c")
        ui = tickBackup(tickBackup(ui))
        ui = hideBackup(ui)
        let again = revealBackup("a b c")
        guard case let .revealed(_, secondsLeft) = again else {
            return XCTFail("expected revealed, got \(again)")
        }
        XCTAssertEqual(backupAutoHideSecs, secondsLeft)
    }

    // MARK: reveal failure copy (Backup.tsx:28-43)

    func testNoMnemonicMapsToTheCorruptedStorageCopy() {
        XCTAssertEqual(
            "Unable to retrieve seed phrase. Your wallet storage may be corrupted.",
            revealErrorMessage(WalletException.NoMnemonic())
        )
    }

    func testOtherRevealFailuresMapToTheRestartCopy() {
        XCTAssertEqual(
            "Unable to retrieve seed phrase. Please restart the app and try again.",
            revealErrorMessage(KotlinRuntimeException(message: "io"))
        )
    }
}
