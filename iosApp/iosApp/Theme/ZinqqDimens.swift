import SwiftUI

/// Layout tokens from the PWA's `index.css` `@theme` (`--spacing-*`) and
/// `Layout.tsx` (U18, KTD-11, R12) — the same table as Android's
/// `ZinqqDimens`.
enum ZinqqDimens {
    /// `--spacing-tab-bar: 64px`.
    static let tabBarHeight: CGFloat = 64

    /// `--spacing-header: 56px`.
    static let headerHeight: CGFloat = 56

    /// `Layout.tsx` `max-w-[430px]`: content column width cap.
    static let contentMaxWidth: CGFloat = 430

    /// WCAG-style minimum touch target used across the PWA (`h-11 w-11`).
    static let minTouchTarget: CGFloat = 44
}

/// The PWA's z-ladder, as SwiftUI zIndex values: tab bar `z-100`, overlays
/// (bottom sheet) `z-300`, and the fenced screen above everything.
enum ZinqqZ {
    static let tabBar: Double = 100
    static let sheet: Double = 300
    static let fenced: Double = 400
}
