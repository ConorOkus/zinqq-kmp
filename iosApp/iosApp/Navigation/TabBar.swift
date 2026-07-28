import SwiftUI

/// The PWA's `TabBar` (U18, KTD-11, R12): a fixed 64pt field-colored bar
/// shown ONLY on Home and Activity — scan icon, WALLET pill, ACTIVITY pill,
/// menu icon (→ Settings). Pills use the uppercase display font; the active
/// pill inverts to `tabActive`/`onTabActive`. All targets are at least 44pt.
struct TabBar: View {
    let current: Route
    let onNavigate: (Route) -> Void

    @Environment(\.zinqqColors) private var colors

    var body: some View {
        HStack(spacing: 0) {
            iconButton(systemName: "qrcode.viewfinder", label: "Scan QR code") {
                onNavigate(.scan)
            }

            TabPill(
                label: "Wallet",
                active: current == .home,
                onTap: { onNavigate(.home) }
            )
            .frame(maxWidth: .infinity)

            TabPill(
                label: "Activity",
                active: current == .activity,
                onTap: { onNavigate(.activity) }
            )
            .frame(maxWidth: .infinity)

            iconButton(systemName: "line.3.horizontal", label: "Settings menu") {
                onNavigate(.settings)
            }
        }
        .padding(.horizontal, 8)
        .frame(height: ZinqqDimens.tabBarHeight)
        .frame(maxWidth: .infinity)
        .background(colors.field.ignoresSafeArea(edges: .bottom))
    }

    private func iconButton(
        systemName: String,
        label: String,
        action: @escaping () -> Void
    ) -> some View {
        Button(action: action) {
            Image(systemName: systemName)
                .font(.system(size: 22))
                .foregroundColor(colors.onField)
                .frame(width: ZinqqDimens.minTouchTarget, height: ZinqqDimens.minTouchTarget)
        }
        .accessibilityLabel(label)
    }
}

private struct TabPill: View {
    let label: String
    let active: Bool
    let onTap: () -> Void

    @Environment(\.zinqqColors) private var colors

    var body: some View {
        Button(action: onTap) {
            Text(label.uppercased())
                .font(ZinqqFont.display(14, weight: .bold))
                .kerning(1)
                .foregroundColor(active ? colors.onTabActive : colors.onFieldMuted)
                .frame(maxWidth: 120)
                .frame(height: ZinqqDimens.minTouchTarget)
                .background(active ? colors.tabActive : colors.field)
                .clipShape(Capsule())
        }
        .accessibilityLabel(label)
        .accessibilityAddTraits(active ? .isSelected : [])
    }
}
