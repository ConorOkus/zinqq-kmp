import SwiftUI
import UIKit

/// Shared scaffolding for the eight settings screens (U22, R12): the PWA's
/// dark-room screen shell, its icon-tile row button, its full-width rounded
/// CTA in the recurring color variants, and the screen-capture shield the
/// Backup reveal uses — mirrors Android's `SettingsCommon.kt`.

/// Dark-room screen: `bg-dark` column under a `ScreenHeader`.
struct SettingsScaffold<Content: View>: View {
    let title: String
    var onBack: (() -> Void)?
    @ViewBuilder var content: Content

    @Environment(\.zinqqColors) private var colors

    var body: some View {
        VStack(spacing: 0) {
            ScreenHeader(title: title, onBack: onBack, tint: colors.onDark)
            content
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .top)
        .background(colors.dark.ignoresSafeArea())
    }
}

/// The Settings/Advanced icon-tile row (`Settings.tsx:122-136`).
struct SettingsRowItem: View {
    let row: SettingsRowSpec
    let systemImage: String
    /// Inert rows (How It Works / Get Help) stay tappable-looking but do
    /// nothing, exactly like the PWA's `route: null` buttons.
    let onClick: (() -> Void)?

    @Environment(\.zinqqColors) private var colors

    var body: some View {
        Button(action: onClick ?? {}) {
            HStack(spacing: 16) {
                ZStack {
                    RoundedRectangle(cornerRadius: 12)
                        .fill(colors.darkElevated)
                        .frame(width: 44, height: 44)
                    Image(systemName: systemImage)
                        .font(.system(size: 18))
                        .foregroundColor(colors.onDarkMuted)
                }
                Text(row.label)
                    .font(ZinqqFont.sans(16, weight: .semibold))
                    .foregroundColor(colors.onDark)
                Spacer()
                Text(row.detail)
                    .font(ZinqqFont.sans(14))
                    .foregroundColor(colors.onDarkMuted)
            }
            .padding(.horizontal, 8)
            .padding(.vertical, 16)
            .contentShape(RoundedRectangle(cornerRadius: 12))
        }
        .accessibilityLabel(row.label)
    }
}

/// Full-width rounded CTA (`rounded-xl px-6 py-4 font-display font-bold`).
struct SettingsCta: View {
    let label: String
    let background: Color
    let contentColor: Color
    let action: () -> Void
    var enabled: Bool = true
    var disabledAlpha: Double = 0.4

    var body: some View {
        Button(action: action) {
            Text(label)
                .font(ZinqqFont.display(17, weight: .bold))
                .foregroundColor(contentColor)
                .frame(maxWidth: .infinity)
                .frame(height: 56)
                .background(background)
                .clipShape(RoundedRectangle(cornerRadius: 12))
                .opacity(enabled ? 1 : disabledAlpha)
        }
        .disabled(!enabled)
        .accessibilityLabel(label)
    }
}

/// Outline CTA (CloseChannel's `Track Progress`/`Force Close Instead` pair).
struct SettingsOutlineCta: View {
    let label: String
    let borderColor: Color
    let contentColor: Color
    let action: () -> Void

    var body: some View {
        Button(action: action) {
            Text(label)
                .font(ZinqqFont.display(17, weight: .bold))
                .foregroundColor(contentColor)
                .frame(maxWidth: .infinity)
                .frame(height: 56)
                .overlay(
                    RoundedRectangle(cornerRadius: 12)
                        .strokeBorder(borderColor, lineWidth: 2)
                )
        }
        .accessibilityLabel(label)
    }
}

/// Centered muted note filling the remaining screen (Loading… etc.).
struct CenteredSettingsNote: View {
    let text: String
    var color: Color?

    @Environment(\.zinqqColors) private var colors

    init(_ text: String, color: Color? = nil) {
        self.text = text
        self.color = color
    }

    var body: some View {
        Text(text)
            .font(ZinqqFont.sans(14))
            .foregroundColor(color ?? colors.onDarkMuted)
            .multilineTextAlignment(.center)
            .padding(.horizontal, 24)
            .frame(maxWidth: .infinity, maxHeight: .infinity)
    }
}

// MARK: - Screen-capture shield (plan U22, R1)

/// Excludes its content from screenshots and screen recordings by hosting it
/// inside the private canvas view of a `UITextField` with
/// `isSecureTextEntry = true` — the OS omits that layer from captures. This
/// is the pragmatic standard iOS equivalent of Android's FLAG_SECURE (iOS has
/// no public "block screenshots" API). Fails OPEN: if the field's private
/// hierarchy ever changes shape, the content renders unshielded rather than
/// blank — the Backup screen's scenePhase hide and its
/// `capturedDidChangeNotification` collapse still cover the app switcher and
/// live captures.
struct CaptureObscured<Content: View>: View {
    @ViewBuilder var content: Content

    var body: some View {
        SecureCaptureHost(content: AnyView(content))
    }
}

private struct SecureCaptureHost: UIViewRepresentable {
    let content: AnyView

    func makeCoordinator() -> Coordinator { Coordinator() }

    final class Coordinator {
        var hosting: UIHostingController<AnyView>?
    }

    func makeUIView(context: Context) -> UIView {
        let hosting = UIHostingController(rootView: content)
        hosting.view.backgroundColor = .clear
        context.coordinator.hosting = hosting

        let field = UITextField()
        field.isSecureTextEntry = true
        field.isUserInteractionEnabled = false
        // The secure field's first sublayer is backed by its private canvas
        // view; content re-parented into it inherits the capture exclusion.
        guard let canvas = field.layer.sublayers?.first?.delegate as? UIView else {
            return hosting.view // fail open (see CaptureObscured)
        }
        canvas.subviews.forEach { $0.removeFromSuperview() }
        canvas.isUserInteractionEnabled = true
        hosting.view.translatesAutoresizingMaskIntoConstraints = false
        canvas.addSubview(hosting.view)
        NSLayoutConstraint.activate([
            hosting.view.topAnchor.constraint(equalTo: canvas.topAnchor),
            hosting.view.bottomAnchor.constraint(equalTo: canvas.bottomAnchor),
            hosting.view.leadingAnchor.constraint(equalTo: canvas.leadingAnchor),
            hosting.view.trailingAnchor.constraint(equalTo: canvas.trailingAnchor),
        ])
        return canvas
    }

    func updateUIView(_ uiView: UIView, context: Context) {
        context.coordinator.hosting?.rootView = content
    }

    func sizeThatFits(
        _ proposal: ProposedViewSize, uiView: UIView, context: Context
    ) -> CGSize? {
        guard let view = context.coordinator.hosting?.view else { return nil }
        let width = proposal.width ?? UIScreen.main.bounds.width
        return view.systemLayoutSizeFitting(
            CGSize(width: width, height: UIView.layoutFittingCompressedSize.height),
            withHorizontalFittingPriority: .required,
            verticalFittingPriority: .fittingSizeLevel
        )
    }
}
