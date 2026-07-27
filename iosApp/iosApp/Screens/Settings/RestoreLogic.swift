import Foundation
import Shared

/// The Restore flow's pure half (U22, F3, R1/R4 UI): the PWA's 12-input grid
/// with paste-fill (`Restore.tsx:27-44`), the validation-gated Continue
/// (`Restore.tsx:29-30`), and the failure copy (`Restore.tsx:74-79,165-168`).
/// The core owns the actual restore (probe → download → clear → write) and
/// emits `RestoreProgress` with the PWA's exact step strings; `WalletModel`
/// owns the stop → restore → restart sequence. Ported from Android's
/// `RestoreLogic.kt` plus the `RestoreUi` reducer from `WalletUiState.kt`.

/// How many grid fields — a BIP39 12-word mnemonic.
let restoreWordCount = 12

/// The step shown the instant restore starts, before the first core event
/// lands — the PWA sets it synchronously (`Restore.tsx:56`).
let restoreInitialStep = "Deriving keys..."

/// One field edited (`Restore.tsx:32-44`): pasting exactly 12
/// whitespace-separated words into ANY field fills the whole grid; anything
/// else edits the target field in place.
func applyWordChange(_ words: [String], index: Int, value: String) -> [String] {
    let pasted = value.split(whereSeparator: { $0.isWhitespace }).map(String.init)
    if pasted.count == restoreWordCount { return pasted }
    var next = words
    next[index] = value
    return next
}

/// The joined candidate mnemonic (`Restore.tsx:29`): trimmed, lowercased.
func mnemonicString(_ words: [String]) -> String {
    words
        .map { $0.trimmingCharacters(in: .whitespacesAndNewlines).lowercased() }
        .joined(separator: " ")
}

/// Continue gating (`Restore.tsx:30`): every field non-blank AND the joined
/// mnemonic validated — natively via the core's `deriveDebugInfo`, the only
/// exported call that checks BIP39 validity (`mnemonicValid` is its result).
func continueEnabled(words: [String], mnemonicValid: Bool) -> Bool {
    words.allSatisfy { !$0.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty }
        && mnemonicValid
}

/// The Restore flow's live phase, owned by `WalletModel` because the model
/// owns the whole stop → restore → restart sequence in its own tasks:
/// leaving the screen mid-restore must not cancel it, and re-entering
/// re-attaches to whatever phase is current (Android's `RestoreUi` twin).
/// `nil` = no restore this session.
enum RestoreUi: Equatable {
    /// `step` is the PWA's exact progress copy from `RestoreProgress` events.
    case inProgress(step: String)
    case succeeded
    case failed(message: String)
}

/// U22/F3 reducer half: the core's restore emits the PWA's exact step copy;
/// it only advances an in-progress restore — a stray late event can neither
/// start one nor resurrect a terminal outcome (Android's `reduce` branch for
/// `Event.RestoreProgress`, tested at the same fidelity).
func reduceRestore(_ restore: RestoreUi?, _ event: WalletEvent) -> RestoreUi? {
    guard case let .restoreProgress(step) = event else { return restore }
    if case .inProgress = restore { return .inProgress(step: step) }
    return restore
}

/// Failure copy: the typed restore errors carry the PWA's strings
/// (`Restore.tsx:74-79` for no-backup, `Restore.tsx:165-168` for the
/// `Restore failed: {message}` catch-all).
func restoreErrorMessage(_ e: KotlinThrowable) -> String {
    switch e {
    case is WalletException.NoBackupFound:
        return "No backup found for this wallet. Make sure you entered the correct seed phrase."
    case let e as WalletException.RestoreFailed:
        return "Restore failed: \(e.detail)"
    case let e as WalletException.BackupInconsistent:
        return "Restore failed: backup inconsistent: \(e.detail)"
    case is WalletException.InvalidMnemonic:
        return "Restore failed: the mnemonic is not a valid BIP39 English 12-word mnemonic"
    case is WalletException.AlreadyRunning:
        return "Restore failed: the node is already running"
    default:
        if let message = e.message, !message.isEmpty { return "Restore failed: \(message)" }
        return "Restore failed: unknown error"
    }
}

/// Bridged variant for the Swift `Error` the async FFI throws.
func restoreErrorMessage(_ error: Error) -> String {
    if let kotlin = kotlinThrowable(error) { return restoreErrorMessage(kotlin) }
    let description = (error as NSError).localizedDescription
    return "Restore failed: \(description.isEmpty ? "unknown error" : description)"
}
