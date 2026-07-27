import SwiftUI

/// Role-named design tokens transcribed exactly from the PWA's `index.css`
/// (U18, KTD-11, R12), hex-for-hex with Android's `ZinqqColors.kt`. Bone +
/// ember scheme: "dark" names the role, not the value — in light mode the
/// dark-room tokens re-point to warm paper.
///
/// `hybrid` holds the `@theme` base table; `dark` and `light` apply the
/// `:root[data-theme='...']` override tables on top of it, exactly as the CSS
/// cascade does.
struct ZinqqColors: Equatable {
    // Brand anchors — bone accent + ember hot moment
    var accent: Color
    var onAccent: Color
    var hot: Color
    var onHot: Color
    // Dark rooms (Send / Receive / Settings)
    var dark: Color
    var darkSurface: Color
    var darkElevated: Color
    var darkBorder: Color
    var onDark: Color
    var onDarkMuted: Color
    // Field screens (Home / Activity / tab bar)
    var field: Color
    var onField: Color
    var onFieldMuted: Color
    var fieldCta: Color
    var onFieldCta: Color
    var fieldOutline: Color
    var tabActive: Color
    var onTabActive: Color
    // Primary action in dark rooms (numpad Next, Share, …)
    var cta: Color
    var onCta: Color
    // Single hot/brand moments
    var amount: Color
    var pill: Color
    var onPill: Color
    var badge: Color
    var onBadge: Color
    var qrTile: Color
    var dotIdle: Color
    // Status
    var danger: Color
    var dangerStrong: Color
    var warning: Color
    var success: Color

    /// Builds a Color from a 24-bit RGB literal so the tables below read
    /// exactly like the CSS hex values they transcribe.
    static func rgb(_ hex: UInt32, alpha: Double = 1) -> Color {
        Color(
            red: Double((hex >> 16) & 0xFF) / 255,
            green: Double((hex >> 8) & 0xFF) / 255,
            blue: Double(hex & 0xFF) / 255,
            opacity: alpha
        )
    }

    /// `@theme` base table (hybrid mode uses it unmodified).
    static let hybrid = ZinqqColors(
        accent: rgb(0xE4D7BE),
        onAccent: rgb(0x1A140A),
        hot: rgb(0xD9481F),
        onHot: rgb(0xFFFFFF),
        dark: rgb(0x12100C),
        darkSurface: rgb(0x1C1913),
        darkElevated: rgb(0x231F18),
        darkBorder: rgb(0x2E2921),
        onDark: rgb(0xF6F0E4),
        onDarkMuted: rgb(0xF6F0E4, alpha: 0.45),
        field: rgb(0xE4D7BE),
        onField: rgb(0x1A140A),
        onFieldMuted: rgb(0x1A140A, alpha: 0.55),
        fieldCta: rgb(0x1A140A),
        onFieldCta: rgb(0xFFFFFF),
        fieldOutline: rgb(0x1A140A),
        tabActive: rgb(0x1A140A),
        onTabActive: rgb(0xE4D7BE),
        cta: rgb(0xF6F0E4),
        onCta: rgb(0x12100C),
        amount: rgb(0xF6F0E4),
        pill: rgb(0xE4D7BE),
        onPill: rgb(0x1A140A),
        badge: rgb(0xE4D7BE),
        onBadge: rgb(0x1A140A),
        qrTile: rgb(0xFFFFFF),
        dotIdle: rgb(0xFFFFFF, alpha: 0.3),
        danger: rgb(0xF87171),
        dangerStrong: rgb(0xDC2626),
        warning: rgb(0xFBBF24),
        success: rgb(0x4ADE80)
    )

    /// `:root[data-theme='dark']` overrides over the base table.
    static let dark: ZinqqColors = {
        var c = hybrid
        c.field = rgb(0x12100C)
        c.onField = rgb(0xF6F0E4)
        c.onFieldMuted = rgb(0xF6F0E4, alpha: 0.45)
        c.fieldCta = rgb(0xD9481F)
        c.fieldOutline = rgb(0xFFFFFF, alpha: 0.22)
        c.tabActive = rgb(0x231F18)
        c.onTabActive = rgb(0xF6F0E4)
        c.cta = rgb(0xD9481F)
        c.onCta = rgb(0xFFFFFF)
        c.amount = rgb(0xD9481F)
        c.pill = rgb(0xD9481F)
        c.onPill = rgb(0xFFFFFF)
        c.dotIdle = rgb(0xFFFFFF, alpha: 0.25)
        return c
    }()

    /// `:root[data-theme='light']` overrides over the base table.
    static let light: ZinqqColors = {
        var c = hybrid
        c.dark = rgb(0xF6F1E5)
        c.darkSurface = rgb(0xEFE8D8)
        c.darkElevated = rgb(0xFCF8F0)
        c.darkBorder = rgb(0x1A140A, alpha: 0.16)
        c.onDark = rgb(0x1A140A)
        c.onDarkMuted = rgb(0x1A140A, alpha: 0.55)
        c.field = rgb(0xF6F1E5)
        c.onField = rgb(0x1A140A)
        c.onFieldMuted = rgb(0x1A140A, alpha: 0.55)
        c.fieldCta = rgb(0xD9481F)
        c.fieldOutline = rgb(0x1A140A, alpha: 0.16)
        c.tabActive = rgb(0x1A140A)
        c.onTabActive = rgb(0xF6F1E5)
        c.cta = rgb(0x1A140A)
        c.onCta = rgb(0xF6F1E5)
        c.amount = rgb(0xD9481F)
        c.pill = rgb(0xD9481F)
        c.onPill = rgb(0xFFFFFF)
        c.badge = rgb(0x1A140A)
        c.onBadge = rgb(0xF6F1E5)
        c.qrTile = rgb(0xFCF8F0)
        c.dotIdle = rgb(0x1A140A, alpha: 0.16)
        c.danger = rgb(0xB42318)
        c.warning = rgb(0xB45309)
        c.success = rgb(0x1B7A3D)
        return c
    }()

    static func forMode(_ mode: AppearanceMode) -> ZinqqColors {
        switch mode {
        case .hybrid: return hybrid
        case .dark: return dark
        case .light: return light
        }
    }
}

// MARK: - Environment (the SwiftUI half of KTD-11's theme environment)

private struct ZinqqColorsKey: EnvironmentKey {
    static let defaultValue = ZinqqColors.hybrid
}

extension EnvironmentValues {
    /// Active token table; AppShell injects `ZinqqColors.forMode(...)` at
    /// scene setup so every view under it reads the persisted mode's table.
    var zinqqColors: ZinqqColors {
        get { self[ZinqqColorsKey.self] }
        set { self[ZinqqColorsKey.self] = newValue }
    }
}
