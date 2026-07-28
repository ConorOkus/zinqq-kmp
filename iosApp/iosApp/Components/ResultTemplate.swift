import SwiftUI

/// The PWA's success/error result screens as one template (U18, KTD-11, R12):
/// centered 80pt circle (badge + check on success, danger/15 + X on failure),
/// headline, optional detail, the load-bearing "Your funds are safe."
/// reassurance on failures, and a full-width CTA. Copy is carried verbatim
/// from the PWA at the call sites (U19–U22).
struct ResultTemplate<Extra: View>: View {
    let success: Bool
    let headline: String
    let onCta: () -> Void
    var detail: String?
    var fundsAreSafe: Bool
    var ctaLabel: String = "Done"
    @ViewBuilder var extraContent: Extra

    @Environment(\.zinqqColors) private var colors

    var body: some View {
        VStack(spacing: 0) {
            Spacer()
            ZStack {
                Circle()
                    .fill(success ? colors.badge : colors.danger.opacity(0.15))
                    .frame(width: 80, height: 80)
                Image(systemName: success ? "checkmark" : "xmark")
                    .font(.system(size: 36, weight: .semibold))
                    .foregroundColor(success ? colors.onBadge : colors.danger)
            }
            .accessibilityLabel(success ? "Success" : "Failure")

            Text(headline)
                .font(ZinqqFont.display(success ? 34 : 24, weight: .bold))
                .foregroundColor(colors.onDark)
                .multilineTextAlignment(.center)
                .padding(.top, 24)

            if let detail {
                Text(detail)
                    .font(ZinqqFont.sans(14))
                    .foregroundColor(success ? colors.onDarkMuted : colors.danger)
                    .multilineTextAlignment(.center)
                    .padding(.top, 8)
            }

            if fundsAreSafe {
                Text("Your funds are safe.")
                    .font(ZinqqFont.sans(14))
                    .foregroundColor(colors.onDarkMuted)
                    .multilineTextAlignment(.center)
                    .padding(.top, 4)
            }

            if Extra.self != EmptyView.self {
                extraContent
                    .padding(.top, 16)
            }

            Button(action: onCta) {
                Text(ctaLabel)
                    .font(ZinqqFont.display(18, weight: .bold))
                    .foregroundColor(colors.onCta)
                    .frame(maxWidth: 280)
                    .frame(height: 56)
                    .background(colors.cta)
                    .clipShape(RoundedRectangle(cornerRadius: 12))
            }
            .padding(.top, 32)
            .accessibilityLabel(ctaLabel)
            Spacer()
        }
        .padding(.horizontal, 32)
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .background(colors.dark.ignoresSafeArea())
    }
}

extension ResultTemplate where Extra == EmptyView {
    init(
        success: Bool,
        headline: String,
        onCta: @escaping () -> Void,
        detail: String? = nil,
        fundsAreSafe: Bool? = nil,
        ctaLabel: String = "Done"
    ) {
        self.init(
            success: success,
            headline: headline,
            onCta: onCta,
            detail: detail,
            fundsAreSafe: fundsAreSafe ?? !success,
            ctaLabel: ctaLabel
        ) { EmptyView() }
    }
}
