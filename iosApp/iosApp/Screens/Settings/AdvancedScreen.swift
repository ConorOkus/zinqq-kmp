import SwiftUI
import UIKit

/// The PWA's Advanced (U22, R12; `Advanced.tsx`): the Node ID copy card with
/// the 2,000 ms "Copied!" flash, then the Balance and Peers rows. The node id
/// is cached on `WalletModel` (fetched on refresh until cached — `nodeId()`
/// needs a running node); like the PWA's not-ready gate, the card simply
/// doesn't render before the first successful start. Mirrors Android's
/// `AdvancedScreen`.
struct AdvancedScreen: View {
    @ObservedObject var model: WalletModel
    var onBack: (() -> Void)?
    let onOpenRow: (Route) -> Void

    @Environment(\.zinqqColors) private var colors
    @State private var copied = false

    var body: some View {
        SettingsScaffold(title: "Advanced", onBack: onBack) {
            ScrollView {
                VStack(spacing: 0) {
                    if let nodeId = model.nodeId {
                        nodeIdCard(nodeId)
                            .padding(.bottom, 16)
                    }
                    ForEach(advancedRows, id: \.label) { row in
                        SettingsRowItem(
                            row: row,
                            systemImage: row.label == "Balance" ? "creditcard" : "person.2",
                            onClick: row.destination.map { destination in
                                { onOpenRow(destination) }
                            }
                        )
                    }
                }
                .padding(16)
            }
        }
        // The PWA's 2,000 ms copied flash (Advanced.tsx:56-62).
        .autoReset($copied, afterMs: 2_000)
    }

    private func nodeIdCard(_ nodeId: String) -> some View {
        Button {
            UIPasteboard.general.string = nodeId
            copied = true
        } label: {
            VStack(alignment: .leading, spacing: 0) {
                Text("Node ID")
                    .font(ZinqqFont.sans(12, weight: .medium))
                    .foregroundColor(colors.onDarkMuted)
                Text(nodeId)
                    .font(.system(size: 12, design: .monospaced))
                    .foregroundColor(colors.onDark)
                    .multilineTextAlignment(.leading)
                    .padding(.top, 4)
                Text(copied ? "Copied!" : "Tap to copy")
                    .font(ZinqqFont.sans(12))
                    .foregroundColor(colors.onDarkMuted)
                    .padding(.top, 8)
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(16)
            .background(colors.darkElevated)
            .clipShape(RoundedRectangle(cornerRadius: 12))
        }
        .accessibilityLabel(copied ? "Node ID copied" : "Copy node ID")
    }
}
