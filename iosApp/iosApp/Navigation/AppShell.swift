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
    /// The Scan screen's raw decode, handed to Send exactly like the PWA's
    /// `location.state` / Android's savedStateHandle (U20, R13): a raw
    /// string, cleared by any navigation that leaves Send, and re-classified
    /// from scratch by the Send screen — never a parsed object.
    @State private var scannedInput: String?
    /// Peers → OpenChannel handoff (U22): the validated `pubkey@host:port`
    /// from the connect form, the PWA's `location.state` / Android's
    /// savedStateHandle twin; missing state redirects back to Peers.
    @State private var openChannelPeer: String?
    /// Peers → CloseChannel handoff (U22): target channel + initial force.
    @State private var closeChannelTarget: CloseChannelTarget?

    struct CloseChannelTarget: Equatable {
        let channelId: String
        let force: Bool
    }

    private var current: Route { path.last ?? .home }

    var body: some View {
        let colors = ZinqqColors.forMode(model.appearanceMode)
        ZStack {
            // The room color fills any letterboxing beyond the 430pt column.
            colors.dark.ignoresSafeArea()

            VStack(spacing: 0) {
                NavigationStack(path: $path) {
                    HomeScreen(model: model, onNavigate: navigate)
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
        // The scan handoff is consumed by exactly one Send visit (U20):
        // navigating anywhere else clears it, so a later Home → Send entry
        // starts fresh. Same discipline for the Peers → Open/Close Channel
        // handoffs (U22).
        if route != .send { scannedInput = nil }
        if route != .peersOpenChannel { openChannelPeer = nil }
        if route != .peersCloseChannel { closeChannelTarget = nil }
        path = route.backChain
    }

    /// Real wallet/activity destinations (U19) plus placeholder bodies for
    /// the remaining routes (U20–U22 replace them); every destination
    /// declares its PWA title and backTo target so headers and back behave.
    @ViewBuilder
    private func destination(_ route: Route) -> some View {
        switch route {
        case .home:
            // Home is the stack root, never pushed; unreachable.
            HomeScreen(model: model, onNavigate: navigate)
        case .receive:
            ReceiveScreen(port: model, onClose: { navigate(.home) })
        case .send:
            SendScreen(
                port: model,
                scannedInput: scannedInput,
                onDone: { navigate(.home) },
                onBackToHome: { navigate(.home) }
            )
        case .scan:
            ScanScreen(
                port: model,
                onScanned: { raw in
                    // Replace Scan with Send and hand over the raw string
                    // (Android's savedStateHandle handoff, R13).
                    scannedInput = raw
                    navigate(.send)
                },
                onClose: { navigate(Route.scan.backTo ?? .home) }
            )
        case .activity:
            ActivityScreen(
                model: model,
                onOpenTx: { navigate(.activityTxDetail(txId: $0)) },
                onOpenClose: { navigate(.activityCloseDetail(channelId: $0)) }
            )
        case let .activityCloseDetail(channelId):
            ChannelCloseDetailScreen(
                model: model,
                channelId: channelId,
                onBack: { navigate(.activity) },
                onRecover: { navigate(.recover) }
            )
        case let .activityTxDetail(txId):
            TransactionDetailScreen(
                model: model,
                txId: txId,
                onBack: { navigate(.activity) },
                onRedirectToClose: { navigate(.activityCloseDetail(channelId: $0)) }
            )
        case .recover:
            RecoverFundsScreen(model: model, onBack: { navigate(.home) })
        case .settings:
            SettingsScreen(
                model: model,
                onBack: backAction(for: route),
                onOpenRow: navigate
            )
        case .settingsBackup:
            BackupScreen(
                port: model,
                onBack: backAction(for: route),
                onDone: { navigate(.settings) }
            )
        case .settingsRestore:
            RestoreScreen(
                model: model,
                onBack: backAction(for: route),
                onRestored: { navigate(.home) }
            )
        case .settingsAdvanced:
            AdvancedScreen(
                model: model,
                onBack: backAction(for: route),
                onOpenRow: navigate
            )
        case .advancedBalance:
            BalanceScreen(model: model, onBack: backAction(for: route))
        case .advancedPeers:
            PeersScreen(
                port: model,
                onBack: backAction(for: route),
                onOpenChannel: { address in
                    openChannelPeer = address
                    navigate(.peersOpenChannel)
                },
                onCloseChannel: { channelId, force in
                    closeChannelTarget = CloseChannelTarget(channelId: channelId, force: force)
                    navigate(.peersCloseChannel)
                }
            )
        case .peersOpenChannel:
            OpenChannelScreen(
                port: model,
                peerAddress: openChannelPeer,
                onBack: { navigate(.advancedPeers) },
                onDone: { navigate(.home) },
                onMissingPeer: { navigate(.advancedPeers) }
            )
        case .peersCloseChannel:
            CloseChannelScreen(
                port: model,
                channelId: closeChannelTarget?.channelId,
                initialForce: closeChannelTarget?.force ?? false,
                onBack: { navigate(.advancedPeers) },
                onTrackProgress: { navigate(.activityCloseDetail(channelId: $0)) },
                onDone: { navigate(.home) },
                onMissingChannel: { navigate(.advancedPeers) }
            )
        }
    }

    /// The declared backTo destination as a navigation closure (nil on the
    /// tab-bar screens, matching the PWA's headers without a back arrow).
    private func backAction(for route: Route) -> (() -> Void)? {
        route.backTo.map { target in { navigate(target) } }
    }
}
