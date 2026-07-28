package zinqq.app.screens.settings

import uniffi.wallet_core.WalletException

/**
 * The Backup reveal machine's pure half (U17, R1 UI): the PWA's warning →
 * revealed → auto-hide cycle (`Backup.tsx:10-13,46-65`) as an immutable
 * state plus reducers the screen drives from a 1-second ticker and the
 * lifecycle observer (the Android equivalent of `visibilitychange`).
 */

/** `AUTO_HIDE_MS = 60_000` (`Backup.tsx:8`), as the countdown's seconds. */
const val BACKUP_AUTO_HIDE_SECS: Int = 60

sealed interface BackupUi {
    /** The write-on-paper warning screen — also the hidden state. */
    data object Warning : BackupUi

    /** The numbered word grid with the live auto-hide countdown. */
    data class Revealed(val words: List<String>, val secondsLeft: Int) : BackupUi

    /** Reveal failed (`Backup.tsx:28-43`). */
    data class Error(val message: String) : BackupUi
}

/** A successful `reveal_mnemonic()` → the grid with a fresh 60 s window. */
fun revealBackup(mnemonic: String): BackupUi = BackupUi.Revealed(
    words = mnemonic.trim().split(Regex("\\s+")),
    secondsLeft = BACKUP_AUTO_HIDE_SECS,
)

/** One second elapsed; hitting zero auto-hides (`Backup.tsx:50-53`). */
fun tickBackup(ui: BackupUi): BackupUi = when (ui) {
    is BackupUi.Revealed ->
        if (ui.secondsLeft <= 1) BackupUi.Warning else ui.copy(secondsLeft = ui.secondsLeft - 1)
    else -> ui
}

/**
 * The screen left the foreground (ON_PAUSE/ON_STOP — the PWA's
 * `visibilitychange` hidden, `Backup.tsx:55-58`): collapse the grid
 * immediately; other states are untouched.
 */
fun hideBackup(ui: BackupUi): BackupUi =
    if (ui is BackupUi.Revealed) BackupUi.Warning else ui

/** `Hides in {countdown}s` (`Backup.tsx:127-129`). */
fun countdownText(secondsLeft: Int): String = "Hides in ${secondsLeft}s"

/**
 * Reveal-failure copy (`Backup.tsx:28-43`): a missing mnemonic is the PWA's
 * null-mnemonic "storage may be corrupted" branch; anything else is its
 * catch-branch "restart the app" copy.
 */
fun revealErrorMessage(e: Throwable): String = when (e) {
    is WalletException.NoMnemonic ->
        "Unable to retrieve seed phrase. Your wallet storage may be corrupted."
    else -> "Unable to retrieve seed phrase. Please restart the app and try again."
}
