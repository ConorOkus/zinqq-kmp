import SwiftUI

/// The PWA's Home-screen banner card pattern (U18, KTD-11, R12), generalized
/// over its variants — `RecoveryBanner`, `PendingSweepBanner`, and the
/// spike's sync-failure line: rounded card on `onField/10`, 36pt icon slot,
/// display-font title, muted subtitle, optional tap-through chevron or
/// dismiss. U19 supplies the real state; copy is carried verbatim from the
/// PWA.
enum BannerIcon {
    case warningHot
    case warning
    case check
}

struct BannerView: View {
    let icon: BannerIcon
    let title: String
    let subtitle: String
    var onTap: (() -> Void)?
    var onDismiss: (() -> Void)?

    @Environment(\.zinqqColors) private var colors

    var body: some View {
        HStack(spacing: 12) {
            Image(systemName: icon == .check ? "checkmark" : "exclamationmark.triangle")
                .font(.system(size: 18))
                .foregroundColor(icon == .warningHot ? colors.hot : colors.onField)
                .frame(width: 36, height: 36)

            VStack(alignment: .leading, spacing: 2) {
                Text(title)
                    .font(ZinqqFont.display(16, weight: .bold))
                    .foregroundColor(colors.onField)
                Text(subtitle)
                    .font(ZinqqFont.sans(12))
                    .foregroundColor(colors.onFieldMuted)
            }
            .frame(maxWidth: .infinity, alignment: .leading)

            if let onDismiss {
                Button(action: onDismiss) {
                    Image(systemName: "xmark")
                        .font(.system(size: 14))
                        .foregroundColor(colors.onFieldMuted)
                        .frame(width: 32, height: 32)
                }
                .accessibilityLabel("Dismiss")
            } else if onTap != nil {
                Image(systemName: "chevron.right")
                    .font(.system(size: 16))
                    .foregroundColor(colors.onFieldMuted)
            }
        }
        .padding(16)
        .frame(maxWidth: .infinity)
        .background(colors.onField.opacity(0.1))
        .clipShape(RoundedRectangle(cornerRadius: 12))
        .contentShape(RoundedRectangle(cornerRadius: 12))
        .onTapGesture { onTap?() }
        .accessibilityElement(children: .combine)
        .accessibilityLabel("\(title). \(subtitle)")
    }
}
