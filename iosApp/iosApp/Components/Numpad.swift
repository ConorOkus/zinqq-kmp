import Shared
import SwiftUI

// MARK: - Shared reducer adapter

/// A key on the sats numpad, as the views speak it. The shared framework's
/// `NumpadKey` protocol hierarchy stays behind `NumpadReducer` (same adapter
/// discipline as `WalletEvent`), but the reduction itself runs in commonMain
/// — 8-digit cap, leading-zero collapse, and backspace behave identically on
/// every client (U18, R12).
enum NumpadInput {
    case digit(Character)
    case backspace
}

enum NumpadReducer {
    /// Numpad amounts cap at 8 digits (₿99,999,999), matching the PWA.
    static let maxDigits = Int(NumpadReducerKt.NUMPAD_MAX_DIGITS)

    /// Pure reduction over a digit-string state, delegated to the shared
    /// `numpadDigitReducer` (the UI-safe R14 carve-out, like `FormatKt`).
    static func reduce(
        _ prev: String,
        _ input: NumpadInput,
        maxDigits: Int = NumpadReducer.maxDigits
    ) -> String {
        let key: NumpadKey
        switch input {
        case let .digit(char):
            guard let scalar = char.unicodeScalars.first, char.isNumber else { return prev }
            key = NumpadKeyDigit(digit: unichar(scalar.value))
        case .backspace:
            key = NumpadKeyBackspace.shared
        }
        return NumpadReducerKt.numpadDigitReducer(
            prev: prev, key: key, maxDigits: Int32(maxDigits)
        )
    }
}

// MARK: - Component

/// The PWA's sats-only `Numpad` (U18, KTD-11, R12): a Next CTA above a 3×4
/// digit grid on the elevated dark surface. Key presses feed the shared
/// reducer at the call site via `NumpadReducer.reduce` (logic in commonMain,
/// pixels here — the R14-style split). All targets are at least 44pt.
struct Numpad: View {
    let onKey: (NumpadInput) -> Void
    let onNext: () -> Void
    let nextEnabled: Bool
    var nextLabel: String = "Next"

    @Environment(\.zinqqColors) private var colors

    private static let rows: [[Character]] = [
        ["1", "2", "3"],
        ["4", "5", "6"],
        ["7", "8", "9"],
    ]

    var body: some View {
        VStack(spacing: 16) {
            nextButton
            VStack(spacing: 8) {
                ForEach(Self.rows, id: \.self) { row in
                    HStack(spacing: 8) {
                        ForEach(row, id: \.self) { digit in
                            digitKey(digit)
                        }
                    }
                }
                HStack(spacing: 8) {
                    Color.clear
                        .frame(maxWidth: .infinity)
                        .frame(height: 64)
                    digitKey("0")
                    backspaceKey
                }
            }
        }
        .padding(.init(top: 16, leading: 24, bottom: 24, trailing: 24))
        .frame(maxWidth: .infinity)
        .background(colors.darkElevated)
        .clipShape(TopRoundedShape(radius: 16))
    }

    private var nextButton: some View {
        Button(action: onNext) {
            HStack(spacing: 8) {
                Text(nextLabel.uppercased())
                    .font(ZinqqFont.display(18, weight: .bold))
                    .kerning(1)
                Image(systemName: "arrow.right")
                    .font(.system(size: 18, weight: .bold))
            }
            .foregroundColor(colors.onCta)
            .frame(maxWidth: .infinity)
            .frame(height: 56)
            .background(colors.cta)
            .clipShape(RoundedRectangle(cornerRadius: 12))
            .opacity(nextEnabled ? 1 : 0.3)
        }
        .disabled(!nextEnabled)
        .accessibilityLabel(nextLabel)
    }

    private func digitKey(_ digit: Character) -> some View {
        Button {
            onKey(.digit(digit))
        } label: {
            Text(String(digit))
                .font(ZinqqFont.display(24, weight: .semibold))
                .foregroundColor(colors.onDark)
                .frame(maxWidth: .infinity)
                .frame(height: 64)
                .contentShape(RoundedRectangle(cornerRadius: 12))
        }
        .accessibilityLabel(String(digit))
    }

    private var backspaceKey: some View {
        Button {
            onKey(.backspace)
        } label: {
            Image(systemName: "delete.backward")
                .font(.system(size: 24))
                .foregroundColor(colors.onDark.opacity(0.7))
                .frame(maxWidth: .infinity)
                .frame(height: 64)
                .contentShape(RoundedRectangle(cornerRadius: 12))
        }
        .accessibilityLabel("Delete")
    }
}
