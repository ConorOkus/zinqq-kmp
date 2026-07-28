package zinqq.app.screens.settings

import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertNull
import kotlin.test.assertTrue
import zinqq.app.balancesFixture
import zinqq.app.nav.Route
import zinqq.app.theme.AppearanceMode

/**
 * Settings/Advanced/Balance pure derivations (U17, R12): the PWA's row
 * tables with How-It-Works/Get-Help preserved as inert no-ops
 * (`Settings.tsx:12-107`, `Advanced.tsx:6-47`), the appearance radiogroup
 * mapping (`Settings.tsx:6-10`, `theme.ts:3`), and the Balance breakdown
 * (`Balance.tsx`, `use-unified-balance.ts`).
 */
class SettingsLogicTest {

    // --- appearance radiogroup (theme.ts:3 order, Settings.tsx labels) ---

    @Test
    fun appearanceModesKeepThePwaOrder() {
        assertEquals(
            listOf(AppearanceMode.HYBRID, AppearanceMode.LIGHT, AppearanceMode.DARK),
            APPEARANCE_MODES,
        )
    }

    @Test
    fun appearanceLabelsMatchThePwa() {
        assertEquals("Hybrid", appearanceLabel(AppearanceMode.HYBRID))
        assertEquals("Light", appearanceLabel(AppearanceMode.LIGHT))
        assertEquals("Dark", appearanceLabel(AppearanceMode.DARK))
    }

    // --- rows (Settings.tsx / Advanced.tsx tables) ---

    @Test
    fun settingsRowsMirrorThePwaTableWithInertHelpRows() {
        assertEquals(
            listOf(
                "Wallet Backup" to "Setup",
                "Recover Wallet" to "From Seed",
                "Advanced" to "Settings",
                "How It Works" to "FAQ",
                "Get Help" to "Chat with us",
            ),
            SETTINGS_ROWS.map { it.label to it.detail },
        )
        assertEquals(Route.SettingsBackup, SETTINGS_ROWS[0].destination)
        assertEquals(Route.SettingsRestore, SETTINGS_ROWS[1].destination)
        assertEquals(Route.SettingsAdvanced, SETTINGS_ROWS[2].destination)
        // The PWA ships these as no-ops (route: null) — replicated inert.
        assertNull(SETTINGS_ROWS[3].destination)
        assertNull(SETTINGS_ROWS[4].destination)
    }

    @Test
    fun advancedRowsMirrorThePwaTable() {
        assertEquals(
            listOf(
                "Balance" to "Onchain · Lightning",
                "Peers" to "Connected",
            ),
            ADVANCED_ROWS.map { it.label to it.detail },
        )
        assertEquals(Route.AdvancedBalance, ADVANCED_ROWS[0].destination)
        assertEquals(Route.AdvancedPeers, ADVANCED_ROWS[1].destination)
    }

    // --- Balance breakdown (use-unified-balance.ts) ---

    @Test
    fun breakdownSplitsOnchainAndFlooredLightning() {
        val breakdown = balanceBreakdown(
            balancesFixture(
                lightningMsat = 2_500_999uL,
                onchainTotalSats = 10_000uL,
                onchainSpendableSats = 8_000uL,
                onchainUntrustedPendingSats = 2_000uL,
            ),
        )
        assertEquals(10_000L, breakdown.onchainSats)
        assertEquals(2_500L, breakdown.lightningSats)
        assertEquals(12_500L, breakdown.totalSats)
        assertEquals(2_000L, breakdown.pendingSats)
    }

    @Test
    fun pendingLineShowsOnlyWhenPositive() {
        assertTrue(balanceBreakdown(balancesFixture()).pendingSats == 0L)
    }
}
