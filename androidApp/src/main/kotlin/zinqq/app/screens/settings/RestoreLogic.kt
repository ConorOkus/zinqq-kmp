package zinqq.app.screens.settings

import uniffi.wallet_core.WalletException

/**
 * The Restore flow's pure half (U17, F3, R1/R4 UI): the PWA's 12-input grid
 * with paste-fill (`Restore.tsx:27-44`), the validation-gated Continue
 * (`Restore.tsx:29-30`), and the failure copy (`Restore.tsx:74-79,165-168`).
 * The core owns the actual restore (probe → download → clear → write) and
 * emits `RestoreProgress` with the PWA's exact step strings; the holder owns
 * the stop → restore → restart sequence.
 */

/** How many grid fields — a BIP39 12-word mnemonic. */
const val RESTORE_WORD_COUNT = 12

/**
 * The step shown the instant restore starts, before the first core event
 * lands — the PWA sets it synchronously (`Restore.tsx:56`).
 */
const val RESTORE_INITIAL_STEP = "Deriving keys..."

/**
 * One field edited (`Restore.tsx:32-44`): pasting exactly 12
 * whitespace-separated words into ANY field fills the whole grid; anything
 * else edits the target field in place.
 */
fun applyWordChange(words: List<String>, index: Int, value: String): List<String> {
    val pasted = value.trim().split(Regex("\\s+"))
    if (pasted.size == RESTORE_WORD_COUNT) return pasted
    return words.toMutableList().also { it[index] = value }
}

/** The joined candidate mnemonic (`Restore.tsx:29`): trimmed, lowercased. */
fun mnemonicString(words: List<String>): String =
    words.joinToString(" ") { it.trim().lowercase() }

/**
 * Continue gating (`Restore.tsx:30`): every field non-blank AND the joined
 * mnemonic validated — natively via the core's `derive_debug_info`, the only
 * exported call that checks BIP39 validity ([mnemonicValid] is its result).
 */
fun continueEnabled(words: List<String>, mnemonicValid: Boolean): Boolean =
    words.all { it.isNotBlank() } && mnemonicValid

/**
 * Failure copy: the typed restore errors carry the PWA's strings
 * (`Restore.tsx:74-79` for no-backup, `Restore.tsx:165-168` for the
 * `Restore failed: {message}` catch-all).
 */
fun restoreErrorMessage(e: Throwable): String = when (e) {
    is WalletException.NoBackupFound ->
        "No backup found for this wallet. Make sure you entered the correct seed phrase."
    is WalletException.RestoreFailed -> "Restore failed: ${e.detail}"
    is WalletException.BackupInconsistent ->
        "Restore failed: backup inconsistent: ${e.detail}"
    is WalletException.InvalidMnemonic ->
        "Restore failed: the mnemonic is not a valid BIP39 English 12-word mnemonic"
    is WalletException.AlreadyRunning -> "Restore failed: the node is already running"
    else ->
        "Restore failed: ${e.message?.takeIf { it.isNotBlank() } ?: "unknown error"}"
}
