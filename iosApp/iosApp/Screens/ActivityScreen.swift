import Shared
import SwiftUI

/// The PWA's Activity page (U19, R11; `Activity.tsx`), mirroring Android's
/// `ActivityScreen`: the merged feed from `listActivity()` (failed Lightning
/// rows already filtered by the core, KTD-7) as direction-icon rows with
/// Pending/close-status badges, relative times, and signed amounts. Field
/// screen; the TabBar is shell-owned (U18).
struct ActivityScreen: View {
    @ObservedObject var model: WalletModel
    let onOpenTx: (String) -> Void
    let onOpenClose: (String) -> Void

    @Environment(\.zinqqColors) private var colors

    var body: some View {
        // One render-time clock for every row, like the PWA's per-render
        // Date.now().
        let nowMs = Int64(Date().timeIntervalSince1970 * 1_000)
        VStack(alignment: .leading, spacing: 0) {
            Text("Activity")
                .font(ZinqqFont.display(30, weight: .bold))
                .foregroundColor(colors.onField)
                .padding(.horizontal, 24)

            if let transactions = model.activity {
                if transactions.isEmpty {
                    CenteredFieldNote("No transactions yet")
                } else {
                    ScrollView {
                        LazyVStack(spacing: 0) {
                            ForEach(transactions, id: \.id) { row in
                                ActivityRowItem(row: row, nowMs: nowMs) {
                                    if row.kind == .channelClose, let channelId = row.channelId {
                                        onOpenClose(channelId)
                                    } else {
                                        onOpenTx(row.id)
                                    }
                                }
                            }
                        }
                        .padding(.top, 24)
                    }
                }
            } else {
                CenteredFieldNote("Loading...")
            }
        }
        .padding(.top, 24)
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
        .background(colors.field.ignoresSafeArea())
    }
}

private struct CenteredFieldNote: View {
    let text: String

    @Environment(\.zinqqColors) private var colors

    init(_ text: String) { self.text = text }

    var body: some View {
        Text(text)
            .font(ZinqqFont.sans(14))
            .foregroundColor(colors.onFieldMuted)
            .frame(maxWidth: .infinity, maxHeight: .infinity)
    }
}

private struct ActivityRowItem: View {
    let row: ActivityRow
    let nowMs: Int64
    let onClick: () -> Void

    @Environment(\.zinqqColors) private var colors

    var body: some View {
        Button(action: onClick) {
            HStack(spacing: 0) {
                Image(
                    systemName: row.direction == ActivityDirection.sent
                        ? "arrow.up.right" : "arrow.down.left"
                )
                    .font(.system(size: 20, weight: .medium))
                    .foregroundColor(colors.onField)
                    .frame(width: 36, height: 36)

                VStack(alignment: .leading, spacing: 2) {
                    HStack(spacing: 8) {
                        Text(activityTitle(row))
                            .font(ZinqqFont.sans(16, weight: .semibold))
                            .foregroundColor(colors.onField)
                        if let badge = activityBadge(row) {
                            Text(badge)
                                .font(ZinqqFont.sans(12))
                                .foregroundColor(colors.onFieldMuted)
                        }
                    }
                    Text(subtitleText)
                        .font(ZinqqFont.sans(12))
                        .foregroundColor(colors.onFieldMuted)
                }
                .padding(.leading, 16)
                .frame(maxWidth: .infinity, alignment: .leading)

                Text(activityAmountText(row))
                    .font(ZinqqFont.display(16, weight: .bold))
                    .foregroundColor(isAmountMuted(row) ? colors.onFieldMuted : colors.onField)
            }
            .padding(.horizontal, 24)
            .padding(.vertical, 16)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
    }

    private var subtitleText: String {
        let time = formatRelativeTime(
            timestampMs: Int64(bitPattern: row.createdAtMs), nowMs: nowMs
        )
        return showsLightningGlyph(row) ? "⚡ \(time)" : time
    }
}
