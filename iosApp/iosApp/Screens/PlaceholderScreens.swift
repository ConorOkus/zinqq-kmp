import SwiftUI

/// Minimal stand-in for every not-yet-built destination (U18): correct
/// header, declared back target, and the right room/field background so all
/// 16 routes are reachable in all three themes. U19–U22 replace these bodies.
struct PlaceholderScreen: View {
    let title: String
    let route: Route
    let onNavigate: (Route) -> Void
    var isFieldScreen: Bool = false

    @Environment(\.zinqqColors) private var colors

    var body: some View {
        let background = isFieldScreen ? colors.field : colors.dark
        let tint = isFieldScreen ? colors.onField : colors.onDark
        let muted = isFieldScreen ? colors.onFieldMuted : colors.onDarkMuted
        VStack(spacing: 0) {
            ScreenHeader(
                title: title,
                onBack: route.backTo.map { target in { onNavigate(target) } },
                tint: tint
            )
            Spacer()
            Text("TODO U19-U22")
                .font(ZinqqFont.sans(14))
                .foregroundColor(muted)
            Spacer()
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .background(background.ignoresSafeArea())
    }
}
