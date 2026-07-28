package zinqq.app.theme

import androidx.compose.material3.Typography
import androidx.compose.runtime.Immutable
import androidx.compose.ui.text.font.Font
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import zinqq.app.R

/**
 * Bundled OFL fonts matching the PWA's self-hosted set (U13, KTD-11, R12):
 * Inter 400–700 as `--font-sans`, Space Grotesk 500–700 as `--font-display`.
 * The TTFs are the exact files the PWA serves from `public/fonts/`, copied
 * into `res/font/` (no Google Fonts CDN, same privacy stance).
 */
val InterFamily = FontFamily(
    Font(R.font.inter_400, FontWeight.Normal),
    Font(R.font.inter_500, FontWeight.Medium),
    Font(R.font.inter_600, FontWeight.SemiBold),
    Font(R.font.inter_700, FontWeight.Bold),
)

val SpaceGroteskFamily = FontFamily(
    Font(R.font.space_grotesk_500, FontWeight.Medium),
    Font(R.font.space_grotesk_600, FontWeight.SemiBold),
    Font(R.font.space_grotesk_700, FontWeight.Bold),
)

/** The two font roles the PWA exposes as CSS variables. */
@Immutable
data class ZinqqFonts(
    /** `--font-sans`: body copy, labels, values. */
    val sans: FontFamily = InterFamily,
    /** `--font-display`: amounts, pills, headings, numpad. */
    val display: FontFamily = SpaceGroteskFamily,
)

/** Material typography with Inter as the app-wide default family. */
internal fun zinqqMaterialTypography(): Typography {
    val default = Typography()
    fun withSans(style: androidx.compose.ui.text.TextStyle) =
        style.copy(fontFamily = InterFamily)
    return Typography(
        displayLarge = withSans(default.displayLarge),
        displayMedium = withSans(default.displayMedium),
        displaySmall = withSans(default.displaySmall),
        headlineLarge = withSans(default.headlineLarge),
        headlineMedium = withSans(default.headlineMedium),
        headlineSmall = withSans(default.headlineSmall),
        titleLarge = withSans(default.titleLarge),
        titleMedium = withSans(default.titleMedium),
        titleSmall = withSans(default.titleSmall),
        bodyLarge = withSans(default.bodyLarge),
        bodyMedium = withSans(default.bodyMedium),
        bodySmall = withSans(default.bodySmall),
        labelLarge = withSans(default.labelLarge),
        labelMedium = withSans(default.labelMedium),
        labelSmall = withSans(default.labelSmall),
    )
}
