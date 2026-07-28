package zinqq.app.screens.settings

import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertIs
import uniffi.wallet_core.WalletException

/**
 * The Backup reveal machine (U17, R1 UI): the PWA's 60-second auto-hide with
 * a live "Hides in Ns" countdown, plus the visibilitychange → lifecycle hide
 * (`Backup.tsx:8,46-65,127-129`) and the reveal-failure copy
 * (`Backup.tsx:28-43`).
 */
class BackupRevealTest {

    @Test
    fun revealSplitsTheMnemonicAndStartsAt60() {
        val ui = revealBackup("abandon ability able about above absent absorb abstract absurd abuse access accident")
        assertIs<BackupUi.Revealed>(ui)
        assertEquals(12, ui.words.size)
        assertEquals("abandon", ui.words.first())
        assertEquals("accident", ui.words.last())
        assertEquals(BACKUP_AUTO_HIDE_SECS, ui.secondsLeft)
    }

    @Test
    fun revealToleratesExtraWhitespace() {
        val ui = revealBackup("  one   two\nthree ")
        assertIs<BackupUi.Revealed>(ui)
        assertEquals(listOf("one", "two", "three"), ui.words)
    }

    @Test
    fun tickCountsDownOneSecondAtATime() {
        var ui: BackupUi = revealBackup("a b c")
        ui = tickBackup(ui)
        assertEquals(59, (ui as BackupUi.Revealed).secondsLeft)
        assertEquals("Hides in 59s", countdownText(ui.secondsLeft))
    }

    @Test
    fun tickAtZeroAutoHides() {
        var ui: BackupUi = BackupUi.Revealed(listOf("a"), secondsLeft = 1)
        ui = tickBackup(ui)
        assertEquals(BackupUi.Warning, ui)
    }

    @Test
    fun tickOnNonRevealedStatesIsIdentity() {
        assertEquals(BackupUi.Warning, tickBackup(BackupUi.Warning))
        val error = BackupUi.Error("boom")
        assertEquals(error, tickBackup(error))
    }

    @Test
    fun lifecycleHideOnlyCollapsesTheRevealedGrid() {
        // ON_PAUSE/ON_STOP while revealed → back to the warning (the PWA's
        // immediate visibilitychange hide).
        assertEquals(BackupUi.Warning, hideBackup(BackupUi.Revealed(listOf("a"), 42)))
        // Warning and error states are untouched by backgrounding.
        assertEquals(BackupUi.Warning, hideBackup(BackupUi.Warning))
        val error = BackupUi.Error("boom")
        assertEquals(error, hideBackup(error))
    }

    @Test
    fun reRevealRestartsTheCountdown() {
        var ui: BackupUi = revealBackup("a b c")
        ui = tickBackup(tickBackup(ui))
        ui = hideBackup(ui)
        val again = revealBackup("a b c")
        assertEquals(BACKUP_AUTO_HIDE_SECS, (again as BackupUi.Revealed).secondsLeft)
    }

    // --- reveal failure copy (Backup.tsx:28-43) ---

    @Test
    fun noMnemonicMapsToTheCorruptedStorageCopy() {
        assertEquals(
            "Unable to retrieve seed phrase. Your wallet storage may be corrupted.",
            revealErrorMessage(WalletException.NoMnemonic()),
        )
    }

    @Test
    fun otherRevealFailuresMapToTheRestartCopy() {
        assertEquals(
            "Unable to retrieve seed phrase. Please restart the app and try again.",
            revealErrorMessage(RuntimeException("io")),
        )
    }
}
