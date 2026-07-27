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
import zinqq.app.screens.RecoverFundsScreen
import zinqq.app.screens.TransactionDetailScreen
import zinqq.app.screens.receive.ReceiveScreen
import zinqq.app.screens.scan.ScanScreen
import zinqq.app.screens.send.SendScreen
import zinqq.app.screens.settings.AdvancedScreen
import zinqq.app.screens.settings.BackupScreen
import zinqq.app.screens.settings.BalanceScreen
import zinqq.app.screens.settings.CloseChannelScreen
import zinqq.app.screens.settings.OpenChannelScreen
import zinqq.app.screens.settings.PeersScreen
import zinqq.app.screens.settings.RestoreScreen
import zinqq.app.screens.settings.SettingsScreen
import zinqq.app.theme.ZinqqDimens
import zinqq.app.theme.ZinqqTheme

/** Saved-state key carrying a scan's raw decode into the Send entry (R13). */
const val SCANNED_INPUT_KEY = "scannedInput"

/**
 * Saved-state keys carrying the Peers screen's selections into the
 * open/close channel entries (U17) — the Android equivalent of the PWA's
 * `location.state` (entry-scoped, consumed once; a missing value redirects
 * back to Peers, like the PWA's replace-navigation guards).
 */
const val OPEN_CHANNEL_PEER_KEY = "openChannelPeer"
const val CLOSE_CHANNEL_ID_KEY = "closeChannelId"
const val CLOSE_CHANNEL_FORCE_KEY = "closeChannelForce"

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
            // The PWA's z-200 overlay ≈ this dedicated route: no TabBar here
            // (only Home/Activity have it) and the fenced screen still
            // renders above (U16, R6 UI).
            ReceiveScreen(
                port = holder,
                onClose = { navController.navigateTo(Route.Receive.backTo ?: Route.Home) },
            )
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
            SettingsScreen(
                holder = holder,
                onBack = backFor(Route.Settings),
                onOpenRow = navController::navigateTo,
            )
        }
        composable(Route.SettingsBackup.pattern) {
            BackupScreen(
                port = holder,
                onBack = backFor(Route.SettingsBackup),
                onDone = { navController.navigateTo(Route.Settings) },
            )
        }
        composable(Route.SettingsRestore.pattern) {
            RestoreScreen(
                holder = holder,
                onBack = backFor(Route.SettingsRestore),
                // F3: success restarts over the restored wallet → Home.
                onRestored = { navController.navigateTo(Route.Home) },
            )
        }
        composable(Route.SettingsAdvanced.pattern) {
            AdvancedScreen(
                holder = holder,
                onBack = backFor(Route.SettingsAdvanced),
                onOpenRow = navController::navigateTo,
            )
        }
        composable(Route.AdvancedBalance.pattern) {
            BalanceScreen(holder = holder, onBack = backFor(Route.AdvancedBalance))
        }
        composable(Route.AdvancedPeers.pattern) {
            PeersScreen(
                port = holder,
                onBack = backFor(Route.AdvancedPeers),
                // The parsed connect input travels like the PWA's
                // location.state: entry-scoped saved state, consumed once.
                onOpenChannel = { address ->
                    navController.navigate(Route.PeersOpenChannel.pattern) {
                        launchSingleTop = true
                        popUpTo(Route.PeersOpenChannel.pattern) { inclusive = true }
                    }
                    navController.getBackStackEntry(Route.PeersOpenChannel.pattern)
                        .savedStateHandle[OPEN_CHANNEL_PEER_KEY] = address
                },
                onCloseChannel = { channelId, force ->
                    navController.navigate(Route.PeersCloseChannel.pattern) {
                        launchSingleTop = true
                        popUpTo(Route.PeersCloseChannel.pattern) { inclusive = true }
                    }
                    val handle = navController
                        .getBackStackEntry(Route.PeersCloseChannel.pattern)
                        .savedStateHandle
                    handle[CLOSE_CHANNEL_ID_KEY] = channelId
                    handle[CLOSE_CHANNEL_FORCE_KEY] = force
                },
            )
        }
        composable(Route.PeersOpenChannel.pattern) { entry ->
            val peerAddress = androidx.compose.runtime.remember(entry) {
                entry.savedStateHandle.remove<String>(OPEN_CHANNEL_PEER_KEY)
            }
            OpenChannelScreen(
                port = holder,
                peerAddress = peerAddress,
                onBack = { navController.navigateTo(Route.AdvancedPeers) },
                onDone = { navController.navigateTo(Route.Home) },
                onMissingPeer = { navController.navigateTo(Route.AdvancedPeers) },
            )
        }
        composable(Route.PeersCloseChannel.pattern) { entry ->
            val channelId = androidx.compose.runtime.remember(entry) {
                entry.savedStateHandle.remove<String>(CLOSE_CHANNEL_ID_KEY)
            }
            val force = androidx.compose.runtime.remember(entry) {
                entry.savedStateHandle.remove<Boolean>(CLOSE_CHANNEL_FORCE_KEY) ?: false
            }
            CloseChannelScreen(
                port = holder,
                channelId = channelId,
                initialForce = force,
                onBack = { navController.navigateTo(Route.AdvancedPeers) },
                onTrackProgress = { id ->
                    navController.navigate(Route.ActivityCloseDetail.path(id)) {
                        launchSingleTop = true
                        popUpTo(Route.PeersCloseChannel.pattern) { inclusive = true }
                    }
                },
                onDone = { navController.navigateTo(Route.Home) },
                onMissingChannel = { navController.navigateTo(Route.AdvancedPeers) },
            )
        }
    }
}
