package zinqq.app.nav

import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertNull

/**
 * The 16-route contract (U13, R12): paths mirror the PWA's `router.tsx` and
 * each screen's declared `backTo` matches the PWA's header links — the
 * NavGraph and system back both navigate from this table.
 */
class RoutesTest {
    @Test
    fun allSixteenPwaRoutesExist() {
        assertEquals(16, Route.all.size)
        assertEquals(
            listOf(
                "home",
                "receive",
                "send",
                "scan",
                "activity",
                "activity/close/{channelId}",
                "activity/{txId}",
                "recover",
                "settings",
                "settings/backup",
                "settings/restore",
                "settings/advanced",
                "settings/advanced/balance",
                "settings/advanced/peers",
                "settings/advanced/peers/open-channel",
                "settings/advanced/peers/close-channel",
            ),
            Route.all.map { it.pattern },
        )
    }

    @Test
    fun backToTargetsMirrorThePwaHeaders() {
        assertNull(Route.Home.backTo)
        assertNull(Route.Activity.backTo)
        assertEquals(Route.Home, Route.Receive.backTo)
        assertEquals(Route.Home, Route.Send.backTo)
        assertEquals(Route.Home, Route.Scan.backTo)
        assertEquals(Route.Home, Route.Recover.backTo)
        assertEquals(Route.Home, Route.Settings.backTo)
        assertEquals(Route.Activity, Route.ActivityCloseDetail.backTo)
        assertEquals(Route.Activity, Route.ActivityTxDetail.backTo)
        assertEquals(Route.Settings, Route.SettingsBackup.backTo)
        assertEquals(Route.Settings, Route.SettingsRestore.backTo)
        assertEquals(Route.Settings, Route.SettingsAdvanced.backTo)
        assertEquals(Route.SettingsAdvanced, Route.AdvancedBalance.backTo)
        assertEquals(Route.SettingsAdvanced, Route.AdvancedPeers.backTo)
        assertEquals(Route.AdvancedPeers, Route.PeersOpenChannel.backTo)
        assertEquals(Route.AdvancedPeers, Route.PeersCloseChannel.backTo)
    }

    @Test
    fun tabBarShowsOnlyOnHomeAndActivity() {
        assertEquals(setOf<Route>(Route.Home, Route.Activity), Route.tabBarRoutes)
    }

    @Test
    fun argumentRoutesBuildConcretePaths() {
        assertEquals("activity/close/abc123", Route.ActivityCloseDetail.path("abc123"))
        assertEquals("activity/deadbeef", Route.ActivityTxDetail.path("deadbeef"))
    }

    @Test
    fun patternLookupResolvesNavBackStackRoutes() {
        assertEquals(Route.Home, Route.fromPattern("home"))
        assertEquals(Route.ActivityTxDetail, Route.fromPattern("activity/{txId}"))
        assertNull(Route.fromPattern("nonsense"))
        assertNull(Route.fromPattern(null))
    }
}
