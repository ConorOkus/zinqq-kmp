package zinqq.app.screens.settings

import uniffi.wallet_core.Balances
import zinqq.app.nav.Route
import zinqq.app.theme.AppearanceMode
import zinqq.spike.msatToSatFloor

/**
 * Pure derivations for the Settings, Advanced, and Balance screens (U17,
 * R12, R14): the PWA's row tables, the appearance radiogroup mapping, and
 * the balance breakdown — screens only place the results. Copy is verbatim
 * from `Settings.tsx`, `Advanced.tsx`, and `Balance.tsx`.
 */

/** One tappable settings row; [destination] `null` = inert no-op (PWA parity). */
data class SettingsRowSpec(
    val label: String,
    val detail: String,
    val destination: Route?,
)

/**
 * `SETTINGS_ITEMS` (`Settings.tsx:12-107`). How It Works and Get Help ship
 * as no-ops in the PWA (`route: null`) — replicated as inert rows (plan
 * Scope Boundaries).
 */
val SETTINGS_ROWS: List<SettingsRowSpec> = listOf(
    SettingsRowSpec("Wallet Backup", "Setup", Route.SettingsBackup),
    SettingsRowSpec("Recover Wallet", "From Seed", Route.SettingsRestore),
    SettingsRowSpec("Advanced", "Settings", Route.SettingsAdvanced),
    SettingsRowSpec("How It Works", "FAQ", null),
    SettingsRowSpec("Get Help", "Chat with us", null),
)

/** `ADVANCED_ITEMS` (`Advanced.tsx:6-47`). */
val ADVANCED_ROWS: List<SettingsRowSpec> = listOf(
    SettingsRowSpec("Balance", "Onchain · Lightning", Route.AdvancedBalance),
    SettingsRowSpec("Peers", "Connected", Route.AdvancedPeers),
)

/** The radiogroup's order: `THEME_MODES` (`theme.ts:3`) — hybrid, light, dark. */
val APPEARANCE_MODES: List<AppearanceMode> = listOf(
    AppearanceMode.HYBRID,
    AppearanceMode.LIGHT,
    AppearanceMode.DARK,
)

/** `THEME_LABELS` (`Settings.tsx:6-10`). */
fun appearanceLabel(mode: AppearanceMode): String = when (mode) {
    AppearanceMode.HYBRID -> "Hybrid"
    AppearanceMode.LIGHT -> "Light"
    AppearanceMode.DARK -> "Dark"
}

/** What the Balance screen renders, per `use-unified-balance.ts`. */
data class BalanceBreakdown(
    /** Full on-chain (confirmed + all pending) + floored Lightning sats. */
    val totalSats: Long,
    /** The `+₿X pending` line: unconfirmed external receives only. */
    val pendingSats: Long,
    val onchainSats: Long,
    val lightningSats: Long,
)

fun balanceBreakdown(balances: Balances): BalanceBreakdown {
    val onchain = balances.onchainTotalSats.toLong()
    val lightning = msatToSatFloor(balances.lightningMsat.toLong())
    return BalanceBreakdown(
        totalSats = onchain + lightning,
        pendingSats = balances.onchainUntrustedPendingSats.toLong(),
        onchainSats = onchain,
        lightningSats = lightning,
    )
}
