import Shared
import SwiftUI
import UIKit

/// The PWA's ChannelCloseDetail (U19, R9/R11; `ChannelCloseDetail.tsx`),
/// mirroring Android's `ChannelCloseDetailScreen`: live-updating dark room —
/// status label, `~₿X` estimate while non-terminal, the "Accessible in ~N
/// days (N blocks)" countdown, a needs-deposit link to Recover when the
/// recovery state names this channel, fact rows, and the per-tx list with
/// role labels, confirmation counts, mempool links, and 1,500ms copy
/// feedback. Renders from `closeDetail(channelId)`, re-queried on every
/// wallet-data refresh.
struct ChannelCloseDetailScreen: View {
    @ObservedObject var model: WalletModel
    let channelId: String
    let onBack: () -> Void
    let onRecover: () -> Void

    @Environment(\.zinqqColors) private var colors

    var body: some View {
        let detail = model.closeDetail.flatMap { $0.channelId == channelId ? $0 : nil }

        VStack(spacing: 0) {
            ScreenHeader(title: "Channel Close", onBack: onBack, tint: colors.onDark)

            if let detail {
                if let record = detail.record {
                    CloseDetailBody(
                        record: record,
                        explorerBaseUrl: model.explorerBaseUrl,
                        needsDepositLink: needsDeposit(model.recoveryState, channelId: channelId),
                        onRecover: onRecover
                    )
                } else {
                    CenteredDarkNote("Close record not found")
                }
            } else {
                CenteredDarkNote("Loading...")
            }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .background(colors.dark.ignoresSafeArea())
        .task(id: channelId) {
            model.loadCloseDetail(channelId: channelId)
        }
    }
}

private struct CloseDetailBody: View {
    let record: CloseRecordView
    let explorerBaseUrl: String
    let needsDepositLink: Bool
    let onRecover: () -> Void

    @Environment(\.zinqqColors) private var colors

    var body: some View {
        let terminal = isTerminalClose(record.status)
        let remaining = blocksRemaining(record)

        ScrollView {
            VStack(spacing: 0) {
                // Hero: live status label + estimated amount.
                VStack(spacing: 8) {
                    Text(closeStatusLabel(record.status))
                        .font(ZinqqFont.sans(18, weight: .semibold))
                        .foregroundColor(colors.onDarkMuted)
                    Text(closeAmountText(record))
                        .font(ZinqqFont.display(36, weight: .bold))
                        .foregroundColor(colors.onDark)
                    if !terminal {
                        Text(nonTerminalCopy(remaining: remaining))
                            .font(ZinqqFont.sans(14))
                            .foregroundColor(colors.onDarkMuted)
                            .multilineTextAlignment(.center)
                    }
                    if record.status == .resolvedUnverified {
                        Text(
                            "The close resolved on-chain, but this wallet couldn't verify "
                                + "receiving the funds — they may have been swept on another device."
                        )
                        .font(ZinqqFont.sans(14))
                        .foregroundColor(colors.warning)
                        .multilineTextAlignment(.center)
                    }
                }
                .padding(.horizontal, 24)
                .padding(.top, 32)
                .padding(.bottom, 24)

                if needsDepositLink {
                    Button(action: onRecover) {
                        Text("A small deposit is needed to recover these funds — tap to continue.")
                            .font(ZinqqFont.sans(14))
                            .foregroundColor(colors.warning)
                            .multilineTextAlignment(.leading)
                            .frame(maxWidth: .infinity, alignment: .leading)
                            .padding(12)
                            .background(colors.warning.opacity(0.1))
                            .clipShape(RoundedRectangle(cornerRadius: 8))
                    }
                    .buttonStyle(.plain)
                    .padding(.horizontal, 24)
                    .padding(.bottom, 16)
                }

                Divider()
                    .overlay(colors.onDark.opacity(0.1))
                    .padding(.horizontal, 24)

                VStack(spacing: 0) {
                    DetailRow(
                        label: "Initiated",
                        value: formatCloseDate(timestampMs: Int64(bitPattern: record.createdAtMs))
                    )
                    if let reason = record.closureReason {
                        DetailRow(label: "Reason", value: reason)
                    }
                    DetailRow(label: "Close type", value: closeTypeLabel(record.closeType))
                    let totalFees = totalFeesSats(record)
                    if terminal && totalFees > 0 {
                        DetailRow(
                            label: "Total fees paid",
                            value: FormatKt.formatBtc(sats: totalFees)
                        )
                    }
                    if let completedAtMs = record.completedAtMs {
                        DetailRow(
                            label: "Completed",
                            value: formatCloseDate(timestampMs: completedAtMs.int64Value)
                        )
                    }
                }
                .padding(.horizontal, 24)
                .padding(.top, 8)

                if !record.txs.isEmpty {
                    VStack(alignment: .leading, spacing: 0) {
                        Text("Transactions")
                            .font(ZinqqFont.sans(14, weight: .medium))
                            .foregroundColor(colors.onDarkMuted)
                        ForEach(Array(record.txs.enumerated()), id: \.element.txid) { index, tx in
                            CloseTxRow(
                                explorerBaseUrl: explorerBaseUrl,
                                tx: tx,
                                lastRow: index == record.txs.count - 1
                            )
                        }
                    }
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(.horizontal, 24)
                    .padding(.top, 16)
                    .padding(.bottom, 24)
                }
            }
        }
    }

    private func nonTerminalCopy(remaining: Int64?) -> String {
        var copy = record.initiator == .remote
            ? "This channel was closed by the network. Your funds are safe "
                + "and return to your wallet automatically."
            : "Your funds return to your wallet automatically."
        if let remaining, remaining > 0 {
            copy += " Accessible in \(humanizeBlocks(remaining)) (\(remaining) blocks)."
        }
        return copy
    }
}

private struct CloseTxRow: View {
    let explorerBaseUrl: String
    let tx: CloseTxView
    let lastRow: Bool

    @Environment(\.zinqqColors) private var colors
    @State private var copied = false

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            HStack {
                Text(closeTxRoleLabel(tx.role))
                    .font(ZinqqFont.sans(14, weight: .semibold))
                    .foregroundColor(colors.onDark)
                    .frame(maxWidth: .infinity, alignment: .leading)
                Text(confirmationText(tx))
                    .font(ZinqqFont.sans(12))
                    .foregroundColor(colors.onDarkMuted)
            }
            HStack {
                Button {
                    if let url = URL(string: explorerTxUrl(explorerBaseUrl, tx.txid)) {
                        UIApplication.shared.open(url)
                    }
                } label: {
                    Text(midTruncate(tx.txid, head: 10, tail: 10, ellipsis: "…"))
                        .font(.system(size: 12, design: .monospaced))
                        .foregroundColor(colors.onDark)
                        .underline()
                }
                .accessibilityLabel("View transaction on mempool.space")
                .frame(maxWidth: .infinity, alignment: .leading)

                Button {
                    UIPasteboard.general.string = tx.txid
                    copied = true
                } label: {
                    Text(copied ? "Copied" : "Copy txid")
                        .font(ZinqqFont.sans(12))
                        .foregroundColor(colors.onDark)
                        .underline()
                }
                .accessibilityLabel("Copy transaction id")
                .padding(.leading, 8)
            }
            // The PWA's 1,500ms "Copied" flash (ChannelCloseDetail.tsx:56-59).
            .autoReset($copied, afterMs: 1_500)
            if let fee = tx.feeSats?.int64Value {
                Text("Fee: \(FormatKt.formatBtc(sats: fee))")
                    .font(ZinqqFont.sans(12))
                    .foregroundColor(colors.onDarkMuted)
            }
            if !lastRow {
                Divider()
                    .overlay(colors.onDark.opacity(0.1))
                    .padding(.top, 12)
            }
        }
        .padding(.vertical, 12)
    }
}
