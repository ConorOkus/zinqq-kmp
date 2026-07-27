import SwiftUI
import UIKit

/// The PWA's `BottomSheet` (U18, KTD-11): scrim + bottom-anchored elevated
/// card with the 200ms slide-up, capped at the 430pt content width, sitting
/// at z-300 in the ladder. Used for the copy-sheet pattern (Receive/Send land
/// in U20/U21). Scrim taps close it at the call site.
struct BottomSheetView<Content: View>: View {
    let open: Bool
    let onClose: () -> Void
    @ViewBuilder let content: Content

    @Environment(\.zinqqColors) private var colors

    var body: some View {
        ZStack(alignment: .bottom) {
            if open {
                // Scrim: black/50, tap-to-close (like the PWA's backdrop).
                Color.black.opacity(0.5)
                    .ignoresSafeArea()
                    .onTapGesture(perform: onClose)
                    .transition(.opacity)
                    .accessibilityLabel("Close")
                    .accessibilityAddTraits(.isButton)

                VStack(alignment: .leading, spacing: 0) {
                    content
                }
                .padding(24)
                .frame(maxWidth: ZinqqDimens.contentMaxWidth)
                .background(colors.darkElevated)
                .clipShape(TopRoundedShape(radius: 16))
                .transition(.move(edge: .bottom))
            }
        }
        .animation(.easeInOut(duration: 0.2), value: open)
        .zIndex(ZinqqZ.sheet)
    }
}

/// Rounds only the top corners (`rounded-t-2xl` in the PWA). SwiftUI's
/// `UnevenRoundedRectangle` needs iOS 16.4; this shape keeps the 16.0
/// deployment target.
struct TopRoundedShape: Shape {
    let radius: CGFloat

    func path(in rect: CGRect) -> Path {
        Path(
            UIBezierPath(
                roundedRect: rect,
                byRoundingCorners: [.topLeft, .topRight],
                cornerRadii: CGSize(width: radius, height: radius)
            ).cgPath
        )
    }
}
