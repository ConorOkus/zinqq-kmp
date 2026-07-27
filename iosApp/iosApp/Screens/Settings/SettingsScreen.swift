import SwiftUI

/// The PWA's Settings (U22, R12; `Settings.tsx`): the five icon rows —
/// How It Works and Get Help preserved as inert no-ops — and the Appearance
/// three-way radiogroup persisted through `WalletModel.appearanceMode`
/// (KTD-11). Mirrors Android's `SettingsScreen`.
struct SettingsScreen: View {
    @ObservedObject var model: WalletModel
    var onBack: (() -> Void)?
    let onOpenRow: (Route) -> Void

    @Environment(\.zinqqColors) private var colors

    var body: some View {
        SettingsScaffold(title: "Settings", onBack: onBack) {
            ScrollView {
                VStack(spacing: 0) {
                    ForEach(settingsRows, id: \.label) { row in
                        SettingsRowItem(
                            row: row,
                            systemImage: Self.rowIcon(row.label),
                            onClick: row.destination.map { destination in
                                { onOpenRow(destination) }
                            }
                        )
                    }

                    appearancePicker
                        .padding(.horizontal, 8)
                        .padding(.top, 24)
                        .padding(.bottom, 16)
                }
                .padding(16)
            }
        }
    }

    /// Appearance radiogroup (`Settings.tsx:138-161`).
    private var appearancePicker: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("Appearance")
                .font(ZinqqFont.sans(16, weight: .semibold))
                .foregroundColor(colors.onDark)
            HStack(spacing: 6) {
                ForEach(appearanceModes, id: \.self) { mode in
                    let selected = model.appearanceMode == mode
                    Button {
                        model.appearanceMode = mode
                    } label: {
                        Text(appearanceLabel(mode))
                            .font(ZinqqFont.sans(14, weight: .semibold))
                            .foregroundColor(selected ? colors.dark : colors.onDarkMuted)
                            .frame(maxWidth: .infinity)
                            .frame(height: 40)
                            .background(selected ? colors.onDark : colors.darkElevated)
                            .clipShape(RoundedRectangle(cornerRadius: 8))
                    }
                    .accessibilityLabel(appearanceLabel(mode))
                    .accessibilityAddTraits(selected ? [.isSelected] : [])
                }
            }
            .padding(4)
            .background(colors.darkElevated)
            .clipShape(RoundedRectangle(cornerRadius: 12))
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }

    private static func rowIcon(_ label: String) -> String {
        switch label {
        case "Wallet Backup": return "lock"
        case "Recover Wallet": return "arrow.counterclockwise"
        case "Advanced": return "gearshape"
        case "How It Works": return "questionmark.circle"
        default: return "bubble.left"
        }
    }
}
