package zinqq.app.screens.settings

import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertFalse
import kotlin.test.assertTrue
import uniffi.wallet_core.Event
import uniffi.wallet_core.WalletException
import zinqq.app.RestoreUi
import zinqq.app.UiState
import zinqq.app.reduce

/**
 * The Restore machine's pure half (U17, F3): the PWA's paste-fill parsing
 * (`Restore.tsx:32-44`), the validation-gated Continue (`Restore.tsx:29-30`),
 * the `RestoreProgress` step progression, and the error mapping onto the
 * PWA's exact copy (`Restore.tsx:74-79,165-168`).
 */
class RestoreLogicTest {

    private val twelve =
        "abandon ability able about above absent absorb abstract absurd abuse access accident"
    private val emptyGrid = List(12) { "" }

    // --- paste-fill matrix ---

    @Test
    fun pastingTwelveWordsIntoTheFirstFieldFillsTheGrid() {
        val words = applyWordChange(emptyGrid, 0, twelve)
        assertEquals(twelve.split(" "), words)
    }

    @Test
    fun pastingTwelveWordsIntoAnyFieldFillsTheGrid() {
        val words = applyWordChange(emptyGrid, 7, twelve)
        assertEquals(twelve.split(" "), words)
    }

    @Test
    fun pasteFillNormalizesExtraWhitespace() {
        val messy = "  abandon\tability  able about\nabove absent absorb abstract absurd abuse access accident  "
        val words = applyWordChange(emptyGrid, 3, messy)
        assertEquals(twelve.split(" "), words)
    }

    @Test
    fun elevenWordsOnlyEditTheTargetField() {
        val eleven = twelve.substringBeforeLast(' ')
        val words = applyWordChange(emptyGrid, 2, eleven)
        assertEquals(eleven, words[2])
        assertEquals("", words[0])
        assertEquals(11, words.count { it.isBlank() })
    }

    @Test
    fun thirteenWordsOnlyEditTheTargetField() {
        val thirteen = "$twelve extra"
        val words = applyWordChange(emptyGrid, 0, thirteen)
        assertEquals(thirteen, words[0])
        assertEquals("", words[1])
    }

    @Test
    fun singleWordTypingEditsInPlace() {
        val words = applyWordChange(emptyGrid, 5, "zoo")
        assertEquals("zoo", words[5])
        assertEquals(11, words.count { it.isEmpty() })
    }

    // --- mnemonic assembly + Continue gating ---

    @Test
    fun mnemonicStringTrimsAndLowercases() {
        val words = List(12) { i -> if (i == 0) " Abandon " else "ability" }
        assertTrue(mnemonicString(words).startsWith("abandon ability"))
    }

    @Test
    fun continueRequiresEveryFieldAndAValidMnemonic() {
        val full = twelve.split(" ")
        assertTrue(continueEnabled(full, mnemonicValid = true))
        assertFalse(continueEnabled(full, mnemonicValid = false))
        val missingOne = full.toMutableList().also { it[11] = " " }
        assertFalse(continueEnabled(missingOne, mnemonicValid = true))
    }

    // --- restore step progression (RestoreProgress events → UiState.restore) ---

    @Test
    fun restoreProgressAdvancesTheInProgressStep() {
        var state = UiState(restore = RestoreUi.InProgress(RESTORE_INITIAL_STEP))
        assertEquals("Deriving keys...", (state.restore as RestoreUi.InProgress).step)
        state = reduce(state, Event.RestoreProgress("Checking backup server..."))
        assertEquals(
            RestoreUi.InProgress("Checking backup server..."),
            state.restore,
        )
        state = reduce(state, Event.RestoreProgress("Downloading 2 item(s)..."))
        assertEquals(RestoreUi.InProgress("Downloading 2 item(s)..."), state.restore)
    }

    @Test
    fun restoreProgressWithoutAnActiveRestoreIsIgnored() {
        val state = reduce(UiState(), Event.RestoreProgress("Deriving keys..."))
        assertEquals(null, state.restore)
    }

    @Test
    fun restoreProgressDoesNotResurrectATerminalOutcome() {
        val failed = UiState(restore = RestoreUi.Failed("boom"))
        assertEquals(failed.restore, reduce(failed, Event.RestoreProgress("late")).restore)
    }

    // --- error mapping (PWA copy) ---

    @Test
    fun noBackupFoundUsesThePwaCopyVerbatim() {
        assertEquals(
            "No backup found for this wallet. Make sure you entered the correct seed phrase.",
            restoreErrorMessage(WalletException.NoBackupFound()),
        )
    }

    @Test
    fun restoreFailedCarriesItsDetail() {
        assertEquals(
            "Restore failed: download interrupted",
            restoreErrorMessage(WalletException.RestoreFailed("download interrupted")),
        )
    }

    @Test
    fun backupInconsistentIsARestoreFailure() {
        assertEquals(
            "Restore failed: backup inconsistent: unexplained remote key",
            restoreErrorMessage(WalletException.BackupInconsistent("unexplained remote key")),
        )
    }

    @Test
    fun unknownFailuresFallBackToTheGenericPrefix() {
        assertEquals(
            "Restore failed: socket reset",
            restoreErrorMessage(RuntimeException("socket reset")),
        )
    }
}
