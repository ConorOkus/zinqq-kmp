import Shared
import XCTest

@testable import iosApp

/// The Restore machine's pure half (U22, F3): the PWA's paste-fill parsing
/// (`Restore.tsx:32-44`), the validation-gated Continue (`Restore.tsx:29-30`),
/// the `RestoreProgress` step progression, and the error mapping onto the
/// PWA's exact copy (`Restore.tsx:74-79,165-168`) — Android's
/// `RestoreLogicTest` ported fixture-for-fixture.
final class RestoreLogicTests: XCTestCase {

    private let twelve =
        "abandon ability able about above absent absorb abstract absurd abuse access accident"
    private let emptyGrid = [String](repeating: "", count: 12)

    // MARK: paste-fill matrix

    func testPastingTwelveWordsIntoTheFirstFieldFillsTheGrid() {
        let words = applyWordChange(emptyGrid, index: 0, value: twelve)
        XCTAssertEqual(twelve.components(separatedBy: " "), words)
    }

    func testPastingTwelveWordsIntoAnyFieldFillsTheGrid() {
        let words = applyWordChange(emptyGrid, index: 7, value: twelve)
        XCTAssertEqual(twelve.components(separatedBy: " "), words)
    }

    func testPasteFillNormalizesExtraWhitespace() {
        let messy =
            "  abandon\tability  able about\nabove absent absorb abstract absurd abuse access accident  "
        let words = applyWordChange(emptyGrid, index: 3, value: messy)
        XCTAssertEqual(twelve.components(separatedBy: " "), words)
    }

    func testElevenWordsOnlyEditTheTargetField() {
        let eleven = twelve.components(separatedBy: " ").dropLast().joined(separator: " ")
        let words = applyWordChange(emptyGrid, index: 2, value: eleven)
        XCTAssertEqual(eleven, words[2])
        XCTAssertEqual("", words[0])
        XCTAssertEqual(11, words.filter { $0.isEmpty }.count)
    }

    func testThirteenWordsOnlyEditTheTargetField() {
        let thirteen = "\(twelve) extra"
        let words = applyWordChange(emptyGrid, index: 0, value: thirteen)
        XCTAssertEqual(thirteen, words[0])
        XCTAssertEqual("", words[1])
    }

    func testSingleWordTypingEditsInPlace() {
        let words = applyWordChange(emptyGrid, index: 5, value: "zoo")
        XCTAssertEqual("zoo", words[5])
        XCTAssertEqual(11, words.filter { $0.isEmpty }.count)
    }

    // MARK: mnemonic assembly + Continue gating

    func testMnemonicStringTrimsAndLowercases() {
        let words = (0..<12).map { $0 == 0 ? " Abandon " : "ability" }
        XCTAssertTrue(mnemonicString(words).hasPrefix("abandon ability"))
    }

    func testContinueRequiresEveryFieldAndAValidMnemonic() {
        let full = twelve.components(separatedBy: " ")
        XCTAssertTrue(continueEnabled(words: full, mnemonicValid: true))
        XCTAssertFalse(continueEnabled(words: full, mnemonicValid: false))
        var missingOne = full
        missingOne[11] = " "
        XCTAssertFalse(continueEnabled(words: missingOne, mnemonicValid: true))
    }

    // MARK: restore step progression (RestoreProgress events → RestoreUi)

    func testRestoreProgressAdvancesTheInProgressStep() {
        var restore: RestoreUi? = .inProgress(step: restoreInitialStep)
        XCTAssertEqual(.inProgress(step: "Deriving keys..."), restore)
        restore = reduceRestore(restore, .restoreProgress(step: "Checking backup server..."))
        XCTAssertEqual(.inProgress(step: "Checking backup server..."), restore)
        restore = reduceRestore(restore, .restoreProgress(step: "Downloading 2 item(s)..."))
        XCTAssertEqual(.inProgress(step: "Downloading 2 item(s)..."), restore)
    }

    func testRestoreProgressWithoutAnActiveRestoreIsIgnored() {
        XCTAssertNil(reduceRestore(nil, .restoreProgress(step: "Deriving keys...")))
    }

    func testRestoreProgressDoesNotResurrectATerminalOutcome() {
        let failed: RestoreUi? = .failed(message: "boom")
        XCTAssertEqual(failed, reduceRestore(failed, .restoreProgress(step: "late")))
    }

    // MARK: error mapping (PWA copy)

    func testNoBackupFoundUsesThePwaCopyVerbatim() {
        XCTAssertEqual(
            "No backup found for this wallet. Make sure you entered the correct seed phrase.",
            restoreErrorMessage(WalletException.NoBackupFound())
        )
    }

    func testRestoreFailedCarriesItsDetail() {
        XCTAssertEqual(
            "Restore failed: download interrupted",
            restoreErrorMessage(WalletException.RestoreFailed(detail: "download interrupted"))
        )
    }

    func testBackupInconsistentIsARestoreFailure() {
        XCTAssertEqual(
            "Restore failed: backup inconsistent: unexplained remote key",
            restoreErrorMessage(
                WalletException.BackupInconsistent(detail: "unexplained remote key")
            )
        )
    }

    func testUnknownFailuresFallBackToTheGenericPrefix() {
        XCTAssertEqual(
            "Restore failed: socket reset",
            restoreErrorMessage(KotlinRuntimeException(message: "socket reset"))
        )
    }
}
