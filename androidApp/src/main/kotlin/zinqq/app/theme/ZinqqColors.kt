package zinqq.app.theme

import androidx.compose.runtime.Immutable
import androidx.compose.ui.graphics.Color

/**
 * Role-named design tokens transcribed exactly from the PWA's `index.css`
 * (U13, KTD-11, R12). Bone + ember scheme: "dark" names the role, not the
 * value — in light mode the dark-room tokens re-point to warm paper.
 *
 * [Hybrid] holds the `@theme` base table; [Dark] and [Light] apply the
 * `:root[data-theme='...']` override tables on top of it, exactly as the CSS
 * cascade does.
 */
@Immutable
data class ZinqqColors(
    // Brand anchors — bone accent + ember hot moment
    val accent: Color,
    val onAccent: Color,
    val hot: Color,
    val onHot: Color,
    // Dark rooms (Send / Receive / Settings)
    val dark: Color,
    val darkSurface: Color,
    val darkElevated: Color,
    val darkBorder: Color,
    val onDark: Color,
    val onDarkMuted: Color,
    // Field screens (Home / Activity / tab bar)
    val field: Color,
    val onField: Color,
    val onFieldMuted: Color,
    val fieldCta: Color,
    val onFieldCta: Color,
    val fieldOutline: Color,
    val tabActive: Color,
    val onTabActive: Color,
    // Primary action in dark rooms (numpad Next, Share, …)
    val cta: Color,
    val onCta: Color,
    // Single hot/brand moments
    val amount: Color,
    val pill: Color,
    val onPill: Color,
    val badge: Color,
    val onBadge: Color,
    val qrTile: Color,
    val dotIdle: Color,
    // Status
    val danger: Color,
    val dangerStrong: Color,
    val warning: Color,
    val success: Color,
) {
    companion object {
        /** `@theme` base table (hybrid mode uses it unmodified). */
        val Hybrid = ZinqqColors(
            accent = Color(0xFFE4D7BE),
            onAccent = Color(0xFF1A140A),
            hot = Color(0xFFD9481F),
            onHot = Color(0xFFFFFFFF),
            dark = Color(0xFF12100C),
            darkSurface = Color(0xFF1C1913),
            darkElevated = Color(0xFF231F18),
            darkBorder = Color(0xFF2E2921),
            onDark = Color(0xFFF6F0E4),
            onDarkMuted = Color(0xFFF6F0E4).copy(alpha = 0.45f),
            field = Color(0xFFE4D7BE),
            onField = Color(0xFF1A140A),
            onFieldMuted = Color(0xFF1A140A).copy(alpha = 0.55f),
            fieldCta = Color(0xFF1A140A),
            onFieldCta = Color(0xFFFFFFFF),
            fieldOutline = Color(0xFF1A140A),
            tabActive = Color(0xFF1A140A),
            onTabActive = Color(0xFFE4D7BE),
            cta = Color(0xFFF6F0E4),
            onCta = Color(0xFF12100C),
            amount = Color(0xFFF6F0E4),
            pill = Color(0xFFE4D7BE),
            onPill = Color(0xFF1A140A),
            badge = Color(0xFFE4D7BE),
            onBadge = Color(0xFF1A140A),
            qrTile = Color(0xFFFFFFFF),
            dotIdle = Color(0xFFFFFFFF).copy(alpha = 0.3f),
            danger = Color(0xFFF87171),
            dangerStrong = Color(0xFFDC2626),
            warning = Color(0xFFFBBF24),
            success = Color(0xFF4ADE80),
        )

        /** `:root[data-theme='dark']` overrides over the base table. */
        val Dark = Hybrid.copy(
            field = Color(0xFF12100C),
            onField = Color(0xFFF6F0E4),
            onFieldMuted = Color(0xFFF6F0E4).copy(alpha = 0.45f),
            fieldCta = Color(0xFFD9481F),
            fieldOutline = Color(0xFFFFFFFF).copy(alpha = 0.22f),
            tabActive = Color(0xFF231F18),
            onTabActive = Color(0xFFF6F0E4),
            cta = Color(0xFFD9481F),
            onCta = Color(0xFFFFFFFF),
            amount = Color(0xFFD9481F),
            pill = Color(0xFFD9481F),
            onPill = Color(0xFFFFFFFF),
            dotIdle = Color(0xFFFFFFFF).copy(alpha = 0.25f),
        )

        /** `:root[data-theme='light']` overrides over the base table. */
        val Light = Hybrid.copy(
            dark = Color(0xFFF6F1E5),
            darkSurface = Color(0xFFEFE8D8),
            darkElevated = Color(0xFFFCF8F0),
            darkBorder = Color(0xFF1A140A).copy(alpha = 0.16f),
            onDark = Color(0xFF1A140A),
            onDarkMuted = Color(0xFF1A140A).copy(alpha = 0.55f),
            field = Color(0xFFF6F1E5),
            onField = Color(0xFF1A140A),
            onFieldMuted = Color(0xFF1A140A).copy(alpha = 0.55f),
            fieldCta = Color(0xFFD9481F),
            fieldOutline = Color(0xFF1A140A).copy(alpha = 0.16f),
            tabActive = Color(0xFF1A140A),
            onTabActive = Color(0xFFF6F1E5),
            cta = Color(0xFF1A140A),
            onCta = Color(0xFFF6F1E5),
            amount = Color(0xFFD9481F),
            pill = Color(0xFFD9481F),
            onPill = Color(0xFFFFFFFF),
            badge = Color(0xFF1A140A),
            onBadge = Color(0xFFF6F1E5),
            qrTile = Color(0xFFFCF8F0),
            dotIdle = Color(0xFF1A140A).copy(alpha = 0.16f),
            danger = Color(0xFFB42318),
            warning = Color(0xFFB45309),
            success = Color(0xFF1B7A3D),
        )

        fun forMode(mode: AppearanceMode): ZinqqColors = when (mode) {
            AppearanceMode.HYBRID -> Hybrid
            AppearanceMode.DARK -> Dark
            AppearanceMode.LIGHT -> Light
        }
    }
}
