import Shared
import SwiftUI
import UIKit

/// The PWA's TransactionDetail (U19, R11; `TransactionDetail.tsx`), mirroring
/// Android's `TransactionDetailScreen`: dark room, hero direction + signed
/// amount, Date/Time (en-GB) / Status / Type rows, and a mempool.space link
/// for on-chain rows. The row is looked up by id from the activity snapshot
/// (the PWA's router-state fast path collapses to the same lookup). Channel
/// closes redirect to their live detail page — a close spans ~14 days and a
/// snapshot would go stale.
struct TransactionDetailScreen: View {
    @ObservedObject var model: WalletModel
    let txId: String
    let onBack: () -> Void
    let onRedirectToClose: (String) -> Void

    @Environment(\.zinqqColors) private var colors

    var body: some View {
        let transactions = model.activity
        let tx = transactions?.first { $0.id == txId }
        let closeChannelId: String? = (tx?.kind == ActivityKind.channelClose) ? tx?.channelId : nil

        VStack(spacing: 0) {
            ScreenHeader(title: "Payment Details", onBack: onBack, tint: colors.onDark)

            if tx == nil && transactions == nil {
                CenteredDarkNote("Loading...")
            } else if tx == nil {
                CenteredDarkNote("Transaction not found")
            } else if closeChannelId != nil {
                Spacer() // redirecting
            } else if let tx {
                detailBody(tx)
            }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .background(colors.dark.ignoresSafeArea())
        .onAppear {
            if let closeChannelId { onRedirectToClose(closeChannelId) }
        }
        .onChange(of: closeChannelId) { channelId in
            if let channelId { onRedirectToClose(channelId) }
        }
    }

    private func detailBody(_ tx: ActivityRow) -> some View {
        let isSent = tx.direction == ActivityDirection.sent
        return VStack(spacing: 0) {
            // Hero: direction + signed amount.
            VStack(spacing: 8) {
                HStack(spacing: 8) {
                    Image(systemName: isSent ? "arrow.up.right" : "arrow.down.left")
                        .font(.system(size: 20, weight: .medium))
                        .foregroundColor(colors.onDarkMuted)
                    Text(isSent ? "Sent" : "Received")
                        .font(ZinqqFont.sans(18, weight: .semibold))
                        .foregroundColor(colors.onDarkMuted)
                }
                Text(activityAmountText(tx))
                    .font(ZinqqFont.display(36, weight: .bold))
                    .foregroundColor(colors.onDark)
            }
            .padding(.horizontal, 24)
            .padding(.top, 32)
            .padding(.bottom, 24)

            Divider()
                .overlay(colors.onDark.opacity(0.1))
                .padding(.horizontal, 24)

            VStack(spacing: 0) {
                let timestamp = Int64(bitPattern: tx.createdAtMs)
                DetailRow(label: "Date", value: formatDetailDate(timestampMs: timestamp))
                DetailRow(label: "Time", value: formatDetailTime(timestampMs: timestamp))
                DetailRow(label: "Status", value: txStatusLabel(tx.status))
                DetailRow(
                    label: "Type",
                    value: tx.kind == .lightning ? "Lightning" : "On-chain"
                )
                if tx.kind == .onchain {
                    ExplorerLinkRow(txid: tx.id)
                }
            }
            .padding(.horizontal, 24)
            .padding(.top, 8)

            Spacer()
        }
    }
}

/// The dark rooms' centered muted note (loading / not-found states).
struct CenteredDarkNote: View {
    let text: String

    @Environment(\.zinqqColors) private var colors

    init(_ text: String) { self.text = text }

    var body: some View {
        Text(text)
            .font(ZinqqFont.sans(14))
            .foregroundColor(colors.onDarkMuted)
            .frame(maxWidth: .infinity, maxHeight: .infinity)
    }
}

/// The PWA's `DetailRow`: muted label left, semibold value right.
struct DetailRow: View {
    let label: String
    let value: String

    @Environment(\.zinqqColors) private var colors

    var body: some View {
        HStack {
            Text(label)
                .font(ZinqqFont.sans(16))
                .foregroundColor(colors.onDarkMuted)
                .frame(maxWidth: .infinity, alignment: .leading)
            Text(value)
                .font(ZinqqFont.sans(16, weight: .semibold))
                .foregroundColor(colors.onDark)
        }
        .padding(.vertical, 12)
    }
}

/// "Transaction" row: mid-truncated txid opening mempool.space externally.
private struct ExplorerLinkRow: View {
    let txid: String

    @Environment(\.zinqqColors) private var colors

    var body: some View {
        HStack {
            Text("Transaction")
                .font(ZinqqFont.sans(16))
                .foregroundColor(colors.onDarkMuted)
                .frame(maxWidth: .infinity, alignment: .leading)
            Button {
                if let url = URL(string: explorerTxUrl(txid)) {
                    UIApplication.shared.open(url)
                }
            } label: {
                Text(midTruncate(txid, head: 8, tail: 8, ellipsis: "..."))
                    .font(ZinqqFont.sans(16, weight: .semibold))
                    .foregroundColor(colors.onDark)
                    .underline()
            }
            .accessibilityLabel("View transaction on mempool.space")
        }
        .padding(.vertical, 12)
    }
}
