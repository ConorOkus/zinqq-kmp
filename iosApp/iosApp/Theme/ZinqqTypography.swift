import SwiftUI

/// Bundled OFL fonts matching the PWA's self-hosted set (U18, KTD-11, R12):
/// Inter 400–700 as `--font-sans`, Space Grotesk 500–700 as `--font-display`.
/// The TTFs are the exact files Android bundles in `res/font/`, copied into
/// `iosApp/Fonts/` (no system fallback for the brand roles, same privacy
/// stance as the PWA's self-hosting).
///
/// Registration mechanism: the TTFs ride into the bundle as plain resources
/// (XcodeGen picks them up from the `iosApp` sources dir) and `UIAppFonts`
/// lists them in the Info.plist written by project.yml's `info:` block —
/// see project.yml for how that merges with GENERATE_INFOPLIST_FILE.
enum ZinqqFont {
    /// `--font-sans`: body copy, labels, values. PostScript names from the
    /// bundled TTFs (Inter-Regular/Medium/SemiBold/Bold).
    static func sans(_ size: CGFloat, weight: Font.Weight = .regular) -> Font {
        .custom(interName(for: weight), size: size)
    }

    /// `--font-display`: amounts, pills, headings, numpad. Space Grotesk ships
    /// 500–700 only; lighter weights clamp to Medium.
    static func display(_ size: CGFloat, weight: Font.Weight = .medium) -> Font {
        .custom(spaceGroteskName(for: weight), size: size)
    }

    private static func interName(for weight: Font.Weight) -> String {
        switch weight {
        case .bold, .heavy, .black: return "Inter-Bold"
        case .semibold: return "Inter-SemiBold"
        case .medium: return "Inter-Medium"
        default: return "Inter-Regular"
        }
    }

    private static func spaceGroteskName(for weight: Font.Weight) -> String {
        switch weight {
        case .bold, .heavy, .black: return "SpaceGrotesk-Bold"
        case .semibold: return "SpaceGrotesk-SemiBold"
        default: return "SpaceGrotesk-Medium"
        }
    }
}
