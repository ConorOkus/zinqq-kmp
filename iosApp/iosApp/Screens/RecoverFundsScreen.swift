import Shared
import SwiftUI
import UIKit

/// The PWA's RecoverFunds (U19, R9; `RecoverFunds.tsx`), mirroring Android's
/// `RecoverFundsScreen`: dark room with the explanation copy, the Stuck
/// balance / Deposit needed card ("Unknown" when the stuck estimate is nil —
/// never a lying zero), a `bitcoin:{address}` QR, the address pill with
/// 1,500ms copy feedback, and the ~14 day timelock notice. Renders the
/// recovery state snapshot; refreshes keep it live.
struct RecoverFundsScreen: View {
    @ObservedObject var model: WalletModel
    let onBack: () -> Void

    @Environment(\.zinqqColors) private var colors
    @State private var copied = false

    var body: some View {
        VStack(spacing: 0) {
            ScreenHeader(title: "Recover Funds", onBack: onBack, tint: colors.onDark)

            if let recovery = model.recoveryState {
                recoveryBody(recovery)
            } else {
                CenteredDarkNote("No recovery needed")
            }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .background(colors.dark.ignoresSafeArea())
        // The PWA's 1,500ms "Copied!" flash (RecoverFunds.tsx:20-21).
        .task(id: copied) {
            if copied {
                try? await Task.sleep(nanoseconds: 1_500_000_000)
                copied = false
            }
        }
    }

    private func recoveryBody(_ recovery: RecoveryStateView) -> some View {
        ScrollView {
            VStack(spacing: 24) {
                Text(
                    "Your payment channel closed unexpectedly. Your funds are safe — "
                        + "a small deposit is needed to move them back to your wallet."
                )
                .font(ZinqqFont.sans(16))
                .foregroundColor(colors.onDarkMuted)
                .multilineTextAlignment(.center)
                .lineSpacing(6)
                .frame(maxWidth: 320)

                // Amounts card.
                VStack(spacing: 16) {
                    AmountCardRow(
                        label: "Stuck balance",
                        value: recovery.stuckBalanceSat.map {
                            FormatKt.formatBtc(sats: $0.int64Value)
                        } ?? "Unknown",
                        valueColor: colors.onDark
                    )
                    Divider().overlay(colors.darkBorder)
                    AmountCardRow(
                        label: "Deposit needed",
                        value: FormatKt.formatBtc(
                            sats: Int64(bitPattern: recovery.depositNeededSat)
                        ),
                        valueColor: colors.amount
                    )
                }
                .padding(.horizontal, 20)
                .padding(.vertical, 16)
                .frame(maxWidth: .infinity)
                .background(colors.darkElevated)
                .clipShape(RoundedRectangle(cornerRadius: 12))

                QrView(
                    payload: "bitcoin:\(recovery.depositAddress)",
                    accessibilityLabel: "QR code for deposit address \(recovery.depositAddress)"
                )
                .frame(width: 200, height: 200)

                // Address pill with the copy button.
                HStack(spacing: 12) {
                    Text(midTruncate(recovery.depositAddress, head: 12, tail: 8, ellipsis: "..."))
                        .font(.system(size: 14, design: .monospaced))
                        .foregroundColor(colors.onDarkMuted)
                        .lineLimit(1)
                    Button {
                        UIPasteboard.general.string = recovery.depositAddress
                        copied = true
                    } label: {
                        HStack(spacing: 6) {
                            if !copied {
                                Image(systemName: "doc.on.doc")
                                    .font(.system(size: 14))
                            }
                            // The PWA's pill copy is CSS-uppercased
                            // ('Copy'/'Copied!').
                            Text(copied ? "COPIED!" : "COPY")
                                .font(ZinqqFont.sans(12, weight: .bold))
                                .kerning(1)
                        }
                        .foregroundColor(colors.onPill)
                        .padding(.horizontal, 12)
                        .padding(.vertical, 6)
                        .background(colors.pill)
                        .clipShape(Capsule())
                    }
                    .accessibilityLabel(copied ? "Copied" : "Copy deposit address")
                }
                .padding(.leading, 20)
                .padding(.trailing, 8)
                .padding(.vertical, 8)
                .background(colors.darkElevated)
                .clipShape(Capsule())

                // Timelock notice.
                HStack(alignment: .top, spacing: 12) {
                    Image(systemName: "clock")
                        .font(.system(size: 20))
                        .foregroundColor(colors.onDarkMuted)
                        .padding(.top, 2)
                    Text("After recovery, funds will be available in approximately 14 days")
                        .font(ZinqqFont.sans(14))
                        .foregroundColor(colors.onDarkMuted)
                }
                .padding(16)
                .frame(maxWidth: .infinity, alignment: .leading)
                .background(colors.darkElevated)
                .clipShape(RoundedRectangle(cornerRadius: 12))
            }
            .padding(.horizontal, 24)
            .padding(.bottom, 32)
        }
    }
}

private struct AmountCardRow: View {
    let label: String
    let value: String
    let valueColor: Color

    @Environment(\.zinqqColors) private var colors

    var body: some View {
        HStack {
            Text(label)
                .font(ZinqqFont.sans(14, weight: .medium))
                .foregroundColor(colors.onDarkMuted)
                .frame(maxWidth: .infinity, alignment: .leading)
            Text(value)
                .font(ZinqqFont.display(18, weight: .bold))
                .foregroundColor(valueColor)
        }
    }
}
