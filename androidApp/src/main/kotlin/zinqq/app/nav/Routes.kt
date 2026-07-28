package zinqq.app.nav

/**
 * The PWA's 16 routes (U13, KTD-11, R12), path-for-path from
 * `src/routes/router.tsx`. Navigation is declarative destination-based like
 * the PWA: each screen names its [backTo] destination and both the header
 * back arrow and system back navigate there — never history pops.
 */
sealed class Route(
    /** Navigation pattern, mirroring the PWA path (placeholders for args). */
    val pattern: String,
    /** Where "back" goes; `null` on the tab-bar screens (Home/Activity). */
    val backTo: Route? = null,
) {
    data object Home : Route("home")
    data object Receive : Route("receive", backTo = Home)
    data object Send : Route("send", backTo = Home)
    data object Scan : Route("scan", backTo = Home)
    data object Activity : Route("activity")

    data object ActivityCloseDetail : Route("activity/close/{channelId}", backTo = Activity) {
        const val ARG_CHANNEL_ID = "channelId"
        fun path(channelId: String) = "activity/close/$channelId"
    }

    data object ActivityTxDetail : Route("activity/{txId}", backTo = Activity) {
        const val ARG_TX_ID = "txId"
        fun path(txId: String) = "activity/$txId"
    }

    data object Recover : Route("recover", backTo = Home)
    data object Settings : Route("settings", backTo = Home)
    data object SettingsBackup : Route("settings/backup", backTo = Settings)
    data object SettingsRestore : Route("settings/restore", backTo = Settings)
    data object SettingsAdvanced : Route("settings/advanced", backTo = Settings)
    data object AdvancedBalance : Route("settings/advanced/balance", backTo = SettingsAdvanced)
    data object AdvancedPeers : Route("settings/advanced/peers", backTo = SettingsAdvanced)
    data object PeersOpenChannel :
        Route("settings/advanced/peers/open-channel", backTo = AdvancedPeers)

    data object PeersCloseChannel :
        Route("settings/advanced/peers/close-channel", backTo = AdvancedPeers)

    companion object {
        // Both lists are lazy on purpose: eager companion vals run during the
        // sealed class's static init, before the nested data objects exist,
        // and would capture nulls.

        /** All 16 destinations, for graph construction and tests. */
        val all: List<Route> by lazy {
            listOf(
                Home, Receive, Send, Scan, Activity, ActivityCloseDetail, ActivityTxDetail,
                Recover, Settings, SettingsBackup, SettingsRestore, SettingsAdvanced,
                AdvancedBalance, AdvancedPeers, PeersOpenChannel, PeersCloseChannel,
            )
        }

        /** TabBar shows only on Home and Activity, like the PWA's `TAB_BAR_ROUTES`. */
        val tabBarRoutes: Set<Route> by lazy { setOf(Home, Activity) }

        fun fromPattern(pattern: String?): Route? = all.firstOrNull { it.pattern == pattern }
    }
}
