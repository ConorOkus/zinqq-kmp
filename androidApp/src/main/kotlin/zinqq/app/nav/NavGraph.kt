package zinqq.app.nav

import androidx.activity.compose.BackHandler
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxHeight
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.widthIn
import androidx.compose.runtime.Composable
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.navigation.NavHostController
import androidx.navigation.compose.NavHost
import androidx.navigation.compose.composable
import androidx.navigation.compose.currentBackStackEntryAsState
import androidx.navigation.compose.rememberNavController
import zinqq.app.WalletHolder
import zinqq.app.components.FencedScreen
import zinqq.app.screens.HomeScreen
import zinqq.app.screens.PlaceholderScreen
import zinqq.app.theme.ZinqqDimens
import zinqq.app.theme.ZinqqTheme

/**
 * Navigation shell (U13, KTD-11, R12): the PWA's `Layout` — a centered
 * ≤430dp column — around a 16-destination NavHost, with the TabBar pinned to
 * the bottom of Home and Activity only, and the fenced screen rendered above
 * every destination when the core fenced itself (plan System-Wide Impact).
 *
 * Back is declarative and destination-based like the PWA's `backTo`: both
 * the header arrow and system back navigate to the declared target with
 * pop-up-to semantics — never back-stack pops.
 */
@Composable
fun ZinqqApp(
    holder: WalletHolder,
    onQuit: () -> Unit,
) {
    val state by holder.state.collectAsState()
    val navController = rememberNavController()
    val backStackEntry by navController.currentBackStackEntryAsState()
    val currentRoute = Route.fromPattern(backStackEntry?.destination?.route)

    Box(
        // The room color fills any letterboxing beyond the 430dp column.
        modifier = Modifier
            .fillMaxSize()
            .background(ZinqqTheme.colors.dark),
        contentAlignment = Alignment.TopCenter,
    ) {
        Column(
            modifier = Modifier
                .fillMaxHeight()
                .widthIn(max = ZinqqDimens.ContentMaxWidth)
                .fillMaxWidth(),
        ) {
            Box(modifier = Modifier.weight(1f)) {
                ZinqqNavHost(navController = navController, holder = holder)
            }
            if (currentRoute != null && currentRoute in Route.tabBarRoutes) {
                TabBar(
                    current = currentRoute,
                    onNavigate = navController::navigateTo,
                )
            }
        }

        // System back mirrors the header's backTo target (KTD-11). Activity is
        // a tab-bar screen with no header back; system back returns to Home.
        // On Home (no target) the handler disables and the platform default
        // applies. Registered after NavHost so it wins over NavHost's popper.
        val systemBackTarget = currentRoute?.backTo
            ?: if (currentRoute == Route.Activity) Route.Home else null
        BackHandler(enabled = !state.fenced && systemBackTarget != null) {
            systemBackTarget?.let(navController::navigateTo)
        }

        // Blocking fenced screen above ALL destinations; the restore-take-over
        // exit lowers it only while the user is on the restore flow (U4).
        if (state.fenced && currentRoute != Route.SettingsRestore) {
            FencedScreen(
                onRestore = { navController.navigateTo(Route.SettingsRestore) },
                onQuit = onQuit,
            )
        }
    }
}

/**
 * Destination-based navigation (KTD-11): go to [route], replacing any
 * existing entry for it instead of stacking history. The back stack stays a
 * straight path of declared destinations, exactly like the PWA's `backTo`
 * links.
 */
fun NavHostController.navigateTo(route: Route) {
    navigate(route.pattern) {
        launchSingleTop = true
        popUpTo(route.pattern) { inclusive = true }
    }
}

@Composable
private fun ZinqqNavHost(
    navController: NavHostController,
    holder: WalletHolder,
) {
    // Placeholder bodies (U14–U17 replace them); every destination declares
    // its PWA title and backTo target so headers and back behave now.
    fun backFor(route: Route): (() -> Unit)? =
        route.backTo?.let { target -> { navController.navigateTo(target) } }

    NavHost(
        navController = navController,
        startDestination = Route.Home.pattern,
    ) {
        composable(Route.Home.pattern) { HomeScreen(holder) }
        composable(Route.Receive.pattern) {
            PlaceholderScreen("Receive", backFor(Route.Receive))
        }
        composable(Route.Send.pattern) {
            PlaceholderScreen("Send", backFor(Route.Send))
        }
        composable(Route.Scan.pattern) {
            PlaceholderScreen("Scan", backFor(Route.Scan))
        }
        composable(Route.Activity.pattern) {
            PlaceholderScreen("Activity", onBack = null, isFieldScreen = true)
        }
        composable(Route.ActivityCloseDetail.pattern) {
            PlaceholderScreen("Channel Close", backFor(Route.ActivityCloseDetail))
        }
        composable(Route.ActivityTxDetail.pattern) {
            PlaceholderScreen("Transaction", backFor(Route.ActivityTxDetail))
        }
        composable(Route.Recover.pattern) {
            PlaceholderScreen("Recover Funds", backFor(Route.Recover))
        }
        composable(Route.Settings.pattern) {
            PlaceholderScreen("Settings", backFor(Route.Settings))
        }
        composable(Route.SettingsBackup.pattern) {
            PlaceholderScreen("Backup", backFor(Route.SettingsBackup))
        }
        composable(Route.SettingsRestore.pattern) {
            PlaceholderScreen("Restore", backFor(Route.SettingsRestore))
        }
        composable(Route.SettingsAdvanced.pattern) {
            PlaceholderScreen("Advanced", backFor(Route.SettingsAdvanced))
        }
        composable(Route.AdvancedBalance.pattern) {
            PlaceholderScreen("Balance", backFor(Route.AdvancedBalance))
        }
        composable(Route.AdvancedPeers.pattern) {
            PlaceholderScreen("Peers", backFor(Route.AdvancedPeers))
        }
        composable(Route.PeersOpenChannel.pattern) {
            PlaceholderScreen("Open Channel", backFor(Route.PeersOpenChannel))
        }
        composable(Route.PeersCloseChannel.pattern) {
            PlaceholderScreen("Close Channel", backFor(Route.PeersCloseChannel))
        }
    }
}
