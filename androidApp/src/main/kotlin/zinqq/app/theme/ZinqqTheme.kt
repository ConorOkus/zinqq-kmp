package zinqq.app.theme

import androidx.compose.material3.MaterialTheme
import androidx.compose.runtime.Composable
import androidx.compose.runtime.CompositionLocalProvider
import androidx.compose.runtime.ReadOnlyComposable
import androidx.compose.runtime.staticCompositionLocalOf
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.dp

/**
 * Compose half of KTD-11's per-platform token system (U13, R12): the PWA's
 * role tokens, three appearance modes, and layout constants encoded once as
 * a theme object. Screens read [ZinqqTheme.colors]/[ZinqqTheme.fonts], never
 * raw hex.
 */

/** Layout tokens from `index.css` `@theme` (`--spacing-*`) and `Layout.tsx`. */
object ZinqqDimens {
    /** `--spacing-tab-bar: 64px`. */
    val TabBarHeight: Dp = 64.dp

    /** `--spacing-header: 56px`. */
    val HeaderHeight: Dp = 56.dp

    /** `Layout.tsx` `max-w-[430px]`: content column width cap. */
    val ContentMaxWidth: Dp = 430.dp

    /** WCAG-style minimum touch target used across the PWA (`h-11 w-11`). */
    val MinTouchTarget: Dp = 44.dp
}

/**
 * The PWA's z-ladder, as Compose zIndex values: tab bar `z-100`, overlays
 * (bottom sheet) `z-300`, and the fenced screen above everything.
 */
object ZinqqZ {
    const val TAB_BAR = 100f
    const val SHEET = 300f
    const val FENCED = 400f
}

val LocalZinqqColors = staticCompositionLocalOf { ZinqqColors.Hybrid }
val LocalZinqqFonts = staticCompositionLocalOf { ZinqqFonts() }
val LocalAppearanceMode = staticCompositionLocalOf { AppearanceMode.DEFAULT }

object ZinqqTheme {
    val colors: ZinqqColors
        @Composable @ReadOnlyComposable get() = LocalZinqqColors.current

    val fonts: ZinqqFonts
        @Composable @ReadOnlyComposable get() = LocalZinqqFonts.current

    val mode: AppearanceMode
        @Composable @ReadOnlyComposable get() = LocalAppearanceMode.current
}

/**
 * Applies the token table for [mode]. The initial mode is read synchronously
 * before the first frame (see `WalletHolder`'s DataStore read), so no frame
 * ever renders in the wrong theme — parity with the PWA's pre-render
 * `data-theme` application.
 */
@Composable
fun ZinqqTheme(
    mode: AppearanceMode,
    content: @Composable () -> Unit,
) {
    CompositionLocalProvider(
        LocalZinqqColors provides ZinqqColors.forMode(mode),
        LocalZinqqFonts provides ZinqqFonts(),
        LocalAppearanceMode provides mode,
    ) {
        MaterialTheme(typography = zinqqMaterialTypography(), content = content)
    }
}
