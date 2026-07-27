import SwiftUI

/// Navigation shell (U18, KTD-11, R12): the PWA's `Layout` — a centered
/// ≤430pt column — around a 16-destination NavigationStack, with the TabBar
/// pinned to the bottom of Home and Activity only, and the fenced screen
/// rendered above every destination when the core fenced itself (plan
/// System-Wide Impact).
///
/// Back is declarative and destination-based like the PWA's `backTo`:
/// `navigate(_:)` replaces the whole path with the destination's backTo
/// chain, so the header arrow and the swipe-back pop agree on where back
/// lands — never history pops.
struct AppShell: View {
    @ObservedObject var model: WalletModel
    @State private var path: [Route] = []

    private var current: Route { path.last ?? .home }

    var body: some View {
        let colors = ZinqqColors.forMode(model.appearanceMode)
        ZStack {
            // The room color fills any letterboxing beyond the 430pt column.
            colors.dark.ignoresSafeArea()

            VStack(spacing: 0) {
                NavigationStack(path: $path) {
                    HomeScreen(model: model)
                        .toolbar(.hidden, for: .navigationBar)
                        .navigationDestination(for: Route.self) { route in
                            destination(route)
                                .navigationBarBackButtonHidden(true)
                                .toolbar(.hidden, for: .navigationBar)
                        }
                }
                if current.showsTabBar {
                    TabBar(current: current, onNavigate: navigate)
                        .zIndex(ZinqqZ.tabBar)
                }
            }
            .frame(maxWidth: ZinqqDimens.contentMaxWidth)

            // Blocking fenced screen above ALL destinations; the
            // restore-take-over exit lowers it only while the user is on the
            // restore flow (U4).
            if model.fenced && current != .settingsRestore {
                FencedScreen(
                    onRestore: { navigate(.settingsRestore) },
                    // "Quit": the other client stays the active one. iOS has
                    // no Android-style finishAndRemoveTask; a clean exit(0)
                    // from this deliberate, user-owned dead end is the
                    // platform-appropriate equivalent (the node is already
                    // halted by the core's fence).
                    onQuit: { exit(0) }
                )
                .zIndex(ZinqqZ.fenced)
            }
        }
        .environment(\.zinqqColors, colors)
    }

    /// Destination-based navigation (KTD-11): the stack becomes the route's
    /// declared backTo chain, exactly like the PWA's `backTo` links.
    private func navigate(_ route: Route) {
        path = route.backChain
    }

    /// Placeholder bodies (U19–U22 replace them); every destination declares
    /// its PWA title and backTo target so headers and back behave now.
    @ViewBuilder
    private func destination(_ route: Route) -> some View {
        switch route {
        case .home:
            // Home is the stack root, never pushed; unreachable.
            HomeScreen(model: model)
        case .receive:
            PlaceholderScreen(title: "Receive", route: route, onNavigate: navigate)
        case .send:
            PlaceholderScreen(title: "Send", route: route, onNavigate: navigate)
        case .scan:
            PlaceholderScreen(title: "Scan", route: route, onNavigate: navigate)
        case .activity:
            PlaceholderScreen(
                title: "Activity", route: route, onNavigate: navigate, isFieldScreen: true
            )
        case .activityCloseDetail:
            PlaceholderScreen(title: "Channel Close", route: route, onNavigate: navigate)
        case .activityTxDetail:
            PlaceholderScreen(title: "Transaction", route: route, onNavigate: navigate)
        case .recover:
            PlaceholderScreen(title: "Recover Funds", route: route, onNavigate: navigate)
        case .settings:
            PlaceholderScreen(title: "Settings", route: route, onNavigate: navigate)
        case .settingsBackup:
            PlaceholderScreen(title: "Backup", route: route, onNavigate: navigate)
        case .settingsRestore:
            PlaceholderScreen(title: "Restore", route: route, onNavigate: navigate)
        case .settingsAdvanced:
            PlaceholderScreen(title: "Advanced", route: route, onNavigate: navigate)
        case .advancedBalance:
            PlaceholderScreen(title: "Balance", route: route, onNavigate: navigate)
        case .advancedPeers:
            PlaceholderScreen(title: "Peers", route: route, onNavigate: navigate)
        case .peersOpenChannel:
            PlaceholderScreen(title: "Open Channel", route: route, onNavigate: navigate)
        case .peersCloseChannel:
            PlaceholderScreen(title: "Close Channel", route: route, onNavigate: navigate)
        }
    }
}
