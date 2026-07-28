import Combine
import SwiftUI
import UIKit

/// The PWA's Wallet Backup (U22, R1 UI; `Backup.tsx`): the write-on-paper
/// warning → "Reveal Seed Phrase" → 2-column numbered word grid with the 60 s
/// "Hides in Ns" auto-hide. Platform-mandated additions (plan U22): the grid
/// hides the instant the scene leaves .active (scenePhase — the PWA's
/// `visibilitychange`, before the app-switcher snapshot is taken), collapses
/// while the screen is being captured (recording/mirroring), and the grid
/// itself renders inside `CaptureObscured`'s secure layer so screenshots and
/// recordings omit it — the iOS twin of Android's FLAG_SECURE.
struct BackupScreen: View {
    let port: any SettingsPort
    var onBack: (() -> Void)?
    let onDone: () -> Void

    @Environment(\.zinqqColors) private var colors
    @Environment(\.scenePhase) private var scenePhase
    @State private var ui: BackupUi = .warning

    private var revealed: Bool {
        if case .revealed = ui { return true }
        return false
    }

    var body: some View {
        SettingsScaffold(title: "Wallet Backup", onBack: onBack) {
            switch ui {
            case .warning:
                warningBody
            case let .revealed(words, secondsLeft):
                revealedBody(words: words, secondsLeft: secondsLeft)
            case let .error(message):
                CenteredSettingsNote(message, color: colors.danger)
            }
        }
        // Lifecycle hide: .inactive covers the app switcher, incoming calls,
        // and lock, before any snapshot is captured (the PWA's
        // document-hidden; Android's ON_PAUSE).
        .onChange(of: scenePhase) { phase in
            if phase != .active { ui = hideBackup(ui) }
        }
        // Live screen recording / AirPlay mirroring: collapse immediately.
        .onReceive(
            NotificationCenter.default.publisher(
                for: UIScreen.capturedDidChangeNotification
            )
        ) { _ in
            if UIScreen.main.isCaptured { ui = hideBackup(ui) }
        }
        // 1-second countdown while revealed; hitting zero auto-hides.
        .task(id: revealed) {
            while case .revealed = ui {
                try? await Task.sleep(nanoseconds: 1_000_000_000)
                ui = tickBackup(ui)
            }
        }
    }

    private var warningBody: some View {
        VStack(spacing: 0) {
            Spacer()
            ZStack {
                Circle()
                    .fill(colors.darkElevated)
                    .frame(width: 64, height: 64)
                Image(systemName: "exclamationmark.triangle")
                    .font(.system(size: 28))
                    .foregroundColor(colors.warning)
            }
            Text("Your recovery phrase is the master key to your wallet.")
                .font(ZinqqFont.display(20, weight: .bold))
                .foregroundColor(colors.onDark)
                .multilineTextAlignment(.center)
                .padding(.top, 24)
            Text(
                "Anyone who has these 12 words can access and steal your funds. "
                    + "Never share them with anyone."
            )
            .font(ZinqqFont.sans(14))
            .foregroundColor(colors.onDarkMuted)
            .multilineTextAlignment(.center)
            .padding(.top, 12)
            VStack(alignment: .leading, spacing: 8) {
                bullet("Write them down on paper and store securely")
                bullet("Do not take a screenshot")
                bullet("Do not copy to clipboard or save digitally")
            }
            .padding(.top, 24)
            SettingsCta(
                label: "Reveal Seed Phrase",
                background: colors.cta,
                contentColor: colors.onCta,
                action: reveal
            )
            .padding(.top, 40)
            Spacer()
        }
        .padding(.horizontal, 16)
        .padding(.bottom, 32)
    }

    private func bullet(_ text: String) -> some View {
        HStack(alignment: .top, spacing: 8) {
            Text("•")
                .font(ZinqqFont.sans(14))
                .foregroundColor(colors.onDarkMuted)
            Text(text)
                .font(ZinqqFont.sans(14))
                .foregroundColor(colors.onDarkMuted)
        }
    }

    private func reveal() {
        Task {
            do {
                ui = revealBackup(try await port.revealMnemonic())
            } catch {
                ui = .error(message: revealErrorMessage(error))
            }
        }
    }

    private func revealedBody(words: [String], secondsLeft: Int) -> some View {
        ScrollView {
            VStack(spacing: 0) {
                HStack {
                    Text("Write down these 12 words in order.")
                        .font(ZinqqFont.sans(14))
                        .foregroundColor(colors.onDarkMuted)
                    Spacer()
                    Text(backupCountdownText(secondsLeft: secondsLeft))
                        .font(ZinqqFont.sans(12))
                        .foregroundColor(colors.onDarkMuted)
                }
                // The words themselves live inside the secure capture layer
                // (screenshots/recordings render it blank).
                CaptureObscured {
                    MnemonicWordGrid(words: words)
                }
                .padding(.top, 24)
                SettingsCta(
                    label: "Done",
                    background: colors.darkElevated,
                    contentColor: colors.onDark,
                    action: onDone
                )
                .padding(.top, 40)
            }
            .padding(.horizontal, 16)
            .padding(.top, 16)
            .padding(.bottom, 32)
        }
    }
}

/// The PWA's `MnemonicWordGrid`: 2 columns of numbered mono word chips.
private struct MnemonicWordGrid: View {
    let words: [String]

    @Environment(\.zinqqColors) private var colors

    var body: some View {
        VStack(spacing: 8) {
            ForEach(Array(stride(from: 0, to: words.count, by: 2)), id: \.self) { rowStart in
                HStack(spacing: 8) {
                    wordChip(index: rowStart)
                    if rowStart + 1 < words.count {
                        wordChip(index: rowStart + 1)
                    } else {
                        Color.clear.frame(maxWidth: .infinity)
                    }
                }
            }
        }
    }

    private func wordChip(index: Int) -> some View {
        HStack(spacing: 8) {
            Text("\(index + 1).")
                .font(.system(size: 14, design: .monospaced))
                .foregroundColor(colors.onDarkMuted)
                .frame(width: 24, alignment: .trailing)
            Text(words[index])
                .font(.system(size: 14, design: .monospaced))
                .foregroundColor(colors.onDark)
            Spacer(minLength: 0)
        }
        .padding(.horizontal, 16)
        .padding(.vertical, 12)
        .frame(maxWidth: .infinity)
        .background(colors.darkElevated)
        .clipShape(RoundedRectangle(cornerRadius: 12))
    }
}
