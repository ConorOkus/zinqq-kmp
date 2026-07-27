import Foundation
import Shared

/// The Backup reveal machine's pure half (U22, R1 UI): the PWA's warning →
/// revealed → auto-hide cycle (`Backup.tsx:10-13,46-65`) as an immutable
/// state plus reducers the screen drives from a 1-second ticker and the
/// scenePhase observer (the iOS equivalent of `visibilitychange`). Ported
/// reducer-for-reducer from Android's `BackupLogic.kt`.

/// `AUTO_HIDE_MS = 60_000` (`Backup.tsx:8`), as the countdown's seconds.
let backupAutoHideSecs = 60

enum BackupUi: Equatable {
    /// The write-on-paper warning screen — also the hidden state.
    case warning

    /// The numbered word grid with the live auto-hide countdown.
    case revealed(words: [String], secondsLeft: Int)

    /// Reveal failed (`Backup.tsx:28-43`).
    case error(message: String)
}

/// A successful `revealMnemonic()` → the grid with a fresh 60 s window.
func revealBackup(_ mnemonic: String) -> BackupUi {
    .revealed(
        words: mnemonic.split(whereSeparator: { $0.isWhitespace }).map(String.init),
        secondsLeft: backupAutoHideSecs
    )
}

/// One second elapsed; hitting zero auto-hides (`Backup.tsx:50-53`).
func tickBackup(_ ui: BackupUi) -> BackupUi {
    guard case let .revealed(words, secondsLeft) = ui else { return ui }
    return secondsLeft <= 1 ? .warning : .revealed(words: words, secondsLeft: secondsLeft - 1)
}

/// The screen left the foreground (scenePhase inactive/background — the
/// PWA's `visibilitychange` hidden, `Backup.tsx:55-58`): collapse the grid
/// immediately; other states are untouched.
func hideBackup(_ ui: BackupUi) -> BackupUi {
    if case .revealed = ui { return .warning }
    return ui
}

/// `Hides in {countdown}s` (`Backup.tsx:127-129`). Named apart from the
/// Receive screen's `countdownText(secondsLeft:)` — an `Int` overload would
/// win literal-argument resolution and silently hijack its call sites.
func backupCountdownText(secondsLeft: Int) -> String { "Hides in \(secondsLeft)s" }

/// Reveal-failure copy (`Backup.tsx:28-43`): a missing mnemonic is the PWA's
/// null-mnemonic "storage may be corrupted" branch; anything else is its
/// catch-branch "restart the app" copy.
func revealErrorMessage(_ e: KotlinThrowable) -> String {
    if e is WalletException.NoMnemonic {
        return "Unable to retrieve seed phrase. Your wallet storage may be corrupted."
    }
    return "Unable to retrieve seed phrase. Please restart the app and try again."
}

/// Bridged variant for the Swift `Error` the async FFI throws.
func revealErrorMessage(_ error: Error) -> String {
    if let kotlin = kotlinThrowable(error) { return revealErrorMessage(kotlin) }
    return "Unable to retrieve seed phrase. Please restart the app and try again."
}
