import SwiftUI

/// The PWA's `ScreenHeader` (U18, KTD-11): fixed 56pt bar, centered title,
/// 44pt back button on the left navigating to the screen's declared `backTo`
/// destination, optional close/right action on the right.
struct ScreenHeader<RightAction: View>: View {
    let title: String
    var onBack: (() -> Void)?
    var onClose: (() -> Void)?
    /// Defaults to `onDark`; field screens pass `onField`.
    var tint: Color?
    @ViewBuilder var rightAction: RightAction

    @Environment(\.zinqqColors) private var colors

    var body: some View {
        let resolvedTint = tint ?? colors.onDark
        ZStack {
            Text(title)
                .font(ZinqqFont.sans(18, weight: .semibold))
                .foregroundColor(resolvedTint)
            HStack {
                if let onBack {
                    Button(action: onBack) {
                        Image(systemName: "chevron.backward")
                            .font(.system(size: 18, weight: .semibold))
                            .foregroundColor(resolvedTint)
                            .frame(
                                width: ZinqqDimens.minTouchTarget,
                                height: ZinqqDimens.minTouchTarget
                            )
                    }
                    .accessibilityLabel("Back")
                }
                Spacer()
                if RightAction.self != EmptyView.self {
                    rightAction
                } else if let onClose {
                    Button(action: onClose) {
                        Image(systemName: "xmark")
                            .font(.system(size: 18, weight: .semibold))
                            .foregroundColor(resolvedTint)
                            .frame(
                                width: ZinqqDimens.minTouchTarget,
                                height: ZinqqDimens.minTouchTarget
                            )
                    }
                    .accessibilityLabel("Close")
                }
            }
            .padding(.horizontal, 16)
        }
        .frame(height: ZinqqDimens.headerHeight)
        .frame(maxWidth: .infinity)
    }
}

extension ScreenHeader where RightAction == EmptyView {
    init(
        title: String,
        onBack: (() -> Void)? = nil,
        onClose: (() -> Void)? = nil,
        tint: Color? = nil
    ) {
        self.init(
            title: title, onBack: onBack, onClose: onClose, tint: tint
        ) { EmptyView() }
    }
}
