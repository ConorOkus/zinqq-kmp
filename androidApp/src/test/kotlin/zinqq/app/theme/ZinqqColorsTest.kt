package zinqq.app.theme

import androidx.compose.ui.graphics.Color
import kotlin.test.Test
import kotlin.test.assertEquals

/**
 * Snapshot-style pins of the three mode tables against `index.css` (U13,
 * KTD-11, R12): the CSS token tables are the spec, these asserts are the
 * transcription check. Spot set per mode: the roles the shell renders first
 * (field/cta/pill families) plus the mode-specific status overrides.
 */
class ZinqqColorsTest {
    @Test
    fun hybridBaseTableMatchesIndexCss() {
        with(ZinqqColors.Hybrid) {
            assertEquals(Color(0xFFE4D7BE), accent)
            assertEquals(Color(0xFFD9481F), hot)
            assertEquals(Color(0xFF12100C), dark)
            assertEquals(Color(0xFF1C1913), darkSurface)
            assertEquals(Color(0xFF231F18), darkElevated)
            assertEquals(Color(0xFF2E2921), darkBorder)
            assertEquals(Color(0xFFF6F0E4), onDark)
            assertEquals(Color(0xFFE4D7BE), field)
            assertEquals(Color(0xFF1A140A), onField)
            assertEquals(Color(0xFF1A140A), fieldCta)
            assertEquals(Color(0xFF1A140A), tabActive)
            assertEquals(Color(0xFFE4D7BE), onTabActive)
            assertEquals(Color(0xFFF6F0E4), cta)
            assertEquals(Color(0xFF12100C), onCta)
            assertEquals(Color(0xFFF6F0E4), amount)
            assertEquals(Color(0xFFE4D7BE), pill)
            assertEquals(Color(0xFF1A140A), onPill)
            assertEquals(Color(0xFFFFFFFF), qrTile)
            assertEquals(Color(0xFFF87171), danger)
            assertEquals(Color(0xFFDC2626), dangerStrong)
            assertEquals(Color(0xFFFBBF24), warning)
            assertEquals(Color(0xFF4ADE80), success)
            assertEquals(Color(0xFF1A140A).copy(alpha = 0.55f), onFieldMuted)
        }
    }

    @Test
    fun darkOverridesMatchIndexCss() {
        with(ZinqqColors.Dark) {
            assertEquals(Color(0xFF12100C), field)
            assertEquals(Color(0xFFF6F0E4), onField)
            assertEquals(Color(0xFFD9481F), fieldCta)
            assertEquals(Color(0xFF231F18), tabActive)
            assertEquals(Color(0xFFF6F0E4), onTabActive)
            assertEquals(Color(0xFFD9481F), cta)
            assertEquals(Color(0xFFFFFFFF), onCta)
            assertEquals(Color(0xFFD9481F), amount)
            assertEquals(Color(0xFFD9481F), pill)
            assertEquals(Color(0xFFFFFFFF), onPill)
            assertEquals(Color(0xFFF6F0E4).copy(alpha = 0.45f), onFieldMuted)
            // Not overridden by the dark table: base values cascade through.
            assertEquals(Color(0xFF12100C), dark)
            assertEquals(Color(0xFFFFFFFF), qrTile)
            assertEquals(Color(0xFFF87171), danger)
        }
    }

    @Test
    fun lightOverridesMatchIndexCss() {
        with(ZinqqColors.Light) {
            assertEquals(Color(0xFFF6F1E5), dark)
            assertEquals(Color(0xFFEFE8D8), darkSurface)
            assertEquals(Color(0xFFFCF8F0), darkElevated)
            assertEquals(Color(0xFF1A140A), onDark)
            assertEquals(Color(0xFFF6F1E5), field)
            assertEquals(Color(0xFF1A140A), onField)
            assertEquals(Color(0xFFD9481F), fieldCta)
            assertEquals(Color(0xFF1A140A), tabActive)
            assertEquals(Color(0xFFF6F1E5), onTabActive)
            assertEquals(Color(0xFF1A140A), cta)
            assertEquals(Color(0xFFF6F1E5), onCta)
            assertEquals(Color(0xFFD9481F), amount)
            assertEquals(Color(0xFFD9481F), pill)
            assertEquals(Color(0xFFFFFFFF), onPill)
            assertEquals(Color(0xFF1A140A), badge)
            assertEquals(Color(0xFFF6F1E5), onBadge)
            assertEquals(Color(0xFFFCF8F0), qrTile)
            assertEquals(Color(0xFFB42318), danger)
            assertEquals(Color(0xFFB45309), warning)
            assertEquals(Color(0xFF1B7A3D), success)
            // dangerStrong has no light override in index.css.
            assertEquals(Color(0xFFDC2626), dangerStrong)
        }
    }

    @Test
    fun forModeSelectsTheMatchingTable() {
        assertEquals(ZinqqColors.Hybrid, ZinqqColors.forMode(AppearanceMode.HYBRID))
        assertEquals(ZinqqColors.Dark, ZinqqColors.forMode(AppearanceMode.DARK))
        assertEquals(ZinqqColors.Light, ZinqqColors.forMode(AppearanceMode.LIGHT))
    }

    @Test
    fun storageValuesMatchThePwaThemeKey() {
        assertEquals("hybrid", AppearanceMode.HYBRID.storageValue)
        assertEquals("dark", AppearanceMode.DARK.storageValue)
        assertEquals("light", AppearanceMode.LIGHT.storageValue)
        assertEquals(AppearanceMode.HYBRID, AppearanceMode.fromStorage(null))
        assertEquals(AppearanceMode.HYBRID, AppearanceMode.fromStorage("solarized"))
        assertEquals(AppearanceMode.DARK, AppearanceMode.fromStorage("dark"))
    }
}
