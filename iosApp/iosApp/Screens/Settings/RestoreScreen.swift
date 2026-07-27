import SwiftUI

/// The PWA's Recover Wallet (U22, F3, R1/R4 UI; `Restore.tsx`): the 12-input
/// 3-column grid with paste-fill into any field, the validation-gated
/// Continue (core `deriveDebugInfo` as the BIP39 check), the destructive
/// "Erase & Restore" confirm, live `RestoreProgress` steps, the PWA error
/// copy, and navigate-Home on success. `WalletModel` owns the stop → restore
/// → restart sequence; this screen renders `RestoreUi` and local input state.
/// Mirrors Android's `RestoreScreen`.
struct RestoreScreen: View {
    @ObservedObject var model: WalletModel
    var onBack: (() -> Void)?
    let onRestored: () -> Void

    @Environment(\.zinqqColors) private var colors
    @State private var words: [String] = Array(repeating: "", count: restoreWordCount)
    @State private var confirming = false
    @State private var mnemonicValid = false

    var body: some View {
        SettingsScaffold(title: "Recover Wallet", onBack: onBack) {
            switch model.restore {
            case let .inProgress(step):
                RestoringBody(step: step)
            case let .failed(message):
                errorBody(message: message)
            case .succeeded:
                // Succeeded navigates away below; render the spinner meanwhile.
                RestoringBody(step: "Restarting wallet...")
            case nil:
                if confirming {
                    confirmBody
                } else {
                    inputBody
                }
            }
        }
        // Re-validate whenever the grid changes; `deriveDebugInfo` is the
        // exported BIP39 check (cheap key derivation, off the MainActor).
        .task(id: words) {
            let candidate = words
            let allFilled = candidate.allSatisfy {
                !$0.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
            }
            guard allFilled else {
                mnemonicValid = false
                return
            }
            mnemonicValid = await model.validateMnemonic(mnemonicString(candidate))
        }
        .onChange(of: model.restore) { restore in
            if restore == .succeeded {
                model.clearRestore()
                onRestored()
            }
        }
    }

    private var inputBody: some View {
        ScrollView {
            VStack(spacing: 0) {
                Text(
                    "Enter your 12-word recovery phrase to restore your wallet from "
                        + "backup. You can paste all 12 words into the first field."
                )
                .font(ZinqqFont.sans(14))
                .foregroundColor(colors.onDarkMuted)
                .frame(maxWidth: .infinity, alignment: .leading)

                // 3-column grid of numbered inputs (Restore.tsx:183-200).
                VStack(spacing: 8) {
                    ForEach(
                        Array(stride(from: 0, to: restoreWordCount, by: 3)), id: \.self
                    ) { rowStart in
                        HStack(spacing: 8) {
                            ForEach(rowStart..<min(rowStart + 3, restoreWordCount), id: \.self) {
                                wordField(index: $0)
                            }
                        }
                    }
                }
                .padding(.top, 16)

                SettingsCta(
                    label: "Continue",
                    background: colors.cta,
                    contentColor: colors.onCta,
                    action: { confirming = true },
                    enabled: continueEnabled(words: words, mnemonicValid: mnemonicValid)
                )
                .padding(.top, 24)
            }
            .padding(.horizontal, 16)
            .padding(.top, 16)
            .padding(.bottom, 32)
        }
    }

    private func wordField(index: Int) -> some View {
        HStack(spacing: 4) {
            Text("\(index + 1)")
                .font(ZinqqFont.sans(12))
                .foregroundColor(colors.onDarkMuted)
                .frame(width: 20, alignment: .trailing)
            TextField(
                "",
                text: Binding(
                    get: { words[index] },
                    set: { words = applyWordChange(words, index: index, value: $0) }
                )
            )
            .font(ZinqqFont.sans(14))
            .foregroundColor(colors.onDark)
            .tint(colors.hot)
            .textInputAutocapitalization(.never)
            .autocorrectionDisabled(true)
            .keyboardType(.asciiCapable)
            .padding(.horizontal, 8)
            .padding(.vertical, 10)
            .background(colors.darkElevated)
            .clipShape(RoundedRectangle(cornerRadius: 8))
            .accessibilityLabel("Word \(index + 1)")
        }
        .frame(maxWidth: .infinity)
    }

    private var confirmBody: some View {
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
            Text("This will replace your current wallet")
                .font(ZinqqFont.display(20, weight: .bold))
                .foregroundColor(colors.onDark)
                .multilineTextAlignment(.center)
                .padding(.top, 24)
            Text(
                "All existing wallet data will be erased and replaced with the "
                    + "restored wallet. Make sure you have backed up your current seed "
                    + "phrase if needed."
            )
            .font(ZinqqFont.sans(14))
            .foregroundColor(colors.onDarkMuted)
            .multilineTextAlignment(.center)
            .padding(.top, 12)
            SettingsCta(
                label: "Erase & Restore",
                background: colors.hot,
                contentColor: colors.onHot,
                action: { model.startRestore(mnemonic: mnemonicString(words)) }
            )
            .padding(.top, 32)
            SettingsCta(
                label: "Cancel",
                background: colors.darkElevated,
                contentColor: colors.onDark,
                action: { confirming = false }
            )
            .padding(.top, 12)
            Spacer()
        }
        .padding(.horizontal, 16)
        .padding(.bottom, 32)
    }

    private func errorBody(message: String) -> some View {
        VStack(spacing: 0) {
            Spacer()
            Text(message)
                .font(ZinqqFont.sans(14))
                .foregroundColor(colors.danger)
                .multilineTextAlignment(.center)
            SettingsCta(
                label: "Try Again",
                background: colors.darkElevated,
                contentColor: colors.onDark,
                action: {
                    confirming = false
                    model.clearRestore()
                }
            )
            .padding(.top, 24)
            Spacer()
        }
        .padding(.horizontal, 24)
    }
}

private struct RestoringBody: View {
    let step: String

    @Environment(\.zinqqColors) private var colors

    var body: some View {
        VStack(spacing: 16) {
            ProgressView()
                .controlSize(.large)
                .tint(colors.onDark)
            Text(step)
                .font(ZinqqFont.sans(14))
                .foregroundColor(colors.onDarkMuted)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }
}
