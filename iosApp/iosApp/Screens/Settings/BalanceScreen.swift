import Shared
import SwiftUI

/// The PWA's Balance (U22, R12; `Balance.tsx`): the Total card with the
/// `+₿X pending` line, then the On-chain / Lightning breakdown — all derived
/// by `balanceBreakdown` from the core's split `balances()` snapshot.
/// Mirrors Android's `BalanceScreen`.
struct BalanceScreen: View {
    @ObservedObject var model: WalletModel
    var onBack: (() -> Void)?

    @Environment(\.zinqqColors) private var colors

    var body: some View {
        SettingsScaffold(title: "Balance", onBack: onBack) {
            if let balances = model.balances {
                content(balanceBreakdown(balances))
            } else {
                CenteredSettingsNote("Loading...")
            }
        }
    }

    private func content(_ breakdown: BalanceBreakdown) -> some View {
        ScrollView {
            VStack(spacing: 16) {
                // Total card (Balance.tsx:24-32).
                VStack(alignment: .leading, spacing: 4) {
                    Text("Total")
                        .font(ZinqqFont.sans(14))
                        .foregroundColor(colors.onDarkMuted)
                    Text(FormatKt.formatBtc(sats: breakdown.totalSats))
                        .font(ZinqqFont.display(30, weight: .bold))
                        .foregroundColor(colors.onDark)
                    if breakdown.pendingSats > 0 {
                        Text("+\(FormatKt.formatBtc(sats: breakdown.pendingSats)) pending")
                            .font(ZinqqFont.sans(14))
                            .foregroundColor(colors.onDarkMuted)
                    }
                }
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(16)
                .background(colors.darkElevated)
                .clipShape(RoundedRectangle(cornerRadius: 12))

                // Breakdown rows (Balance.tsx:35-76).
                breakdownRow(
                    label: "On-chain",
                    amountSats: breakdown.onchainSats,
                    systemImage: "bitcoinsign.circle",
                    iconTint: ZinqqColors.rgb(0xFB923C), // orange-400
                    iconBackground: ZinqqColors.rgb(0xF97316, alpha: 0.2) // orange-500/20
                )
                breakdownRow(
                    label: "Lightning",
                    amountSats: breakdown.lightningSats,
                    systemImage: "bolt.fill",
                    iconTint: ZinqqColors.rgb(0xFACC15), // yellow-400
                    iconBackground: ZinqqColors.rgb(0xEAB308, alpha: 0.2) // yellow-500/20
                )
            }
            .padding(.horizontal, 24)
            .padding(.top, 16)
            .padding(.bottom, 32)
        }
    }

    private func breakdownRow(
        label: String,
        amountSats: Int64,
        systemImage: String,
        iconTint: Color,
        iconBackground: Color
    ) -> some View {
        HStack(spacing: 12) {
            ZStack {
                RoundedRectangle(cornerRadius: 8)
                    .fill(iconBackground)
                    .frame(width: 36, height: 36)
                Image(systemName: systemImage)
                    .font(.system(size: 16))
                    .foregroundColor(iconTint)
            }
            Text(label)
                .font(ZinqqFont.sans(16, weight: .semibold))
                .foregroundColor(colors.onDark)
            Spacer()
            Text(FormatKt.formatBtc(sats: amountSats))
                .font(ZinqqFont.display(18, weight: .bold))
                .foregroundColor(colors.onDark)
        }
        .padding(16)
        .background(colors.darkElevated)
        .clipShape(RoundedRectangle(cornerRadius: 12))
    }
}
