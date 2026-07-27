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
import zinqq.app.screens.ActivityScreen
import zinqq.app.screens.ChannelCloseDetailScreen
import zinqq.app.screens.HomeScreen
import zinqq.app.screens.PlaceholderScreen
import zinqq.app.screens.RecoverFundsScreen
import zinqq.app.screens.TransactionDetailScreen
import zinqq.app.screens.scan.ScanScreen
import zinqq.app.screens.send.SendScreen
import zinqq.app.theme.ZinqqDimens
import zinqq.app.theme.ZinqqTheme

/** Saved-state key carrying a scan's raw decode into the Send entry (R13). */
const val SCANNED_INPUT_KEY = "scannedInput"

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
        composable(Route.Home.pattern) {
            HomeScreen(
                holder = holder,
                onSend = { navController.navigateTo(Route.Send) },
                onRequest = { navController.navigateTo(Route.Receive) },
                onRecover = { navController.navigateTo(Route.Recover) },
                onReceive = { navController.navigateTo(Route.Receive) },
            )
        }
        composable(Route.Receive.pattern) {
            PlaceholderScreen("Receive", backFor(Route.Receive))
        }
        composable(Route.Send.pattern) { entry ->
            // The scanned raw string travels like the PWA's location.state
            // (Scan.tsx:60 / Send.tsx:608-620): entry-scoped, consumed once,
            // and re-classified from scratch — never a parsed object (R13/R14).
            val scanned = androidx.compose.runtime.remember(entry) {
                entry.savedStateHandle.remove<String>(SCANNED_INPUT_KEY)
            }
            SendScreen(
                port = holder,
                scannedInput = scanned,
                onDone = { navController.navigateTo(Route.Home) },
                onBackToHome = { navController.navigateTo(Route.Home) },
            )
        }
        composable(Route.Scan.pattern) {
            ScanScreen(
                port = holder,
                onScanned = { raw ->
                    // Replace Scan with Send (popUpTo) and hand over the raw
                    // string via the Send entry's saved state.
                    navController.navigate(Route.Send.pattern) {
                        launchSingleTop = true
                        popUpTo(Route.Scan.pattern) { inclusive = true }
                    }
                    navController.getBackStackEntry(Route.Send.pattern)
                        .savedStateHandle[SCANNED_INPUT_KEY] = raw
                },
                onClose = { navController.navigateTo(Route.Scan.backTo ?: Route.Home) },
            )
        }
        composable(Route.Activity.pattern) {
            ActivityScreen(
                holder = holder,
                onOpenTx = { txId ->
                    navController.navigate(Route.ActivityTxDetail.path(txId)) {
                        launchSingleTop = true
                    }
                },
                onOpenClose = { channelId ->
                    navController.navigate(Route.ActivityCloseDetail.path(channelId)) {
                        launchSingleTop = true
                    }
                },
            )
        }
        composable(Route.ActivityCloseDetail.pattern) { entry ->
            ChannelCloseDetailScreen(
                holder = holder,
                channelId = entry.arguments
                    ?.getString(Route.ActivityCloseDetail.ARG_CHANNEL_ID)
                    .orEmpty(),
                onBack = { navController.navigateTo(Route.Activity) },
                onRecover = { navController.navigateTo(Route.Recover) },
            )
        }
        composable(Route.ActivityTxDetail.pattern) { entry ->
            TransactionDetailScreen(
                holder = holder,
                txId = entry.arguments?.getString(Route.ActivityTxDetail.ARG_TX_ID).orEmpty(),
                onBack = { navController.navigateTo(Route.Activity) },
                // A close spans ~14 days; its own live detail page replaces
                // this snapshot view (TransactionDetail.tsx:81-85).
                onRedirectToClose = { channelId ->
                    navController.navigate(Route.ActivityCloseDetail.path(channelId)) {
                        launchSingleTop = true
                        popUpTo(Route.ActivityTxDetail.pattern) { inclusive = true }
                    }
                },
            )
        }
        composable(Route.Recover.pattern) {
            RecoverFundsScreen(
                holder = holder,
                onBack = { navController.navigateTo(Route.Home) },
            )
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
