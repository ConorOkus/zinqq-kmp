import Foundation

/// The PWA's 16 routes (U18, KTD-11, R12), path-for-path from
/// `src/routes/router.tsx` and case-for-case with Android's `Routes.kt`.
/// Navigation is declarative destination-based like the PWA: each screen
/// names its `backTo` destination and both the header back arrow and the
/// interactive pop gesture land there — never history pops.
enum Route: Hashable {
    case home
    case receive
    case send
    case scan
    case activity
    case activityCloseDetail(channelId: String)
    case activityTxDetail(txId: String)
    case recover
    case settings
    case settingsBackup
    case settingsRestore
    case settingsAdvanced
    case advancedBalance
    case advancedPeers
    case peersOpenChannel
    case peersCloseChannel

    /// Navigation pattern, mirroring the PWA path (placeholders for args).
    var pattern: String {
        switch self {
        case .home: return "home"
        case .receive: return "receive"
        case .send: return "send"
        case .scan: return "scan"
        case .activity: return "activity"
        case .activityCloseDetail: return "activity/close/{channelId}"
        case .activityTxDetail: return "activity/{txId}"
        case .recover: return "recover"
        case .settings: return "settings"
        case .settingsBackup: return "settings/backup"
        case .settingsRestore: return "settings/restore"
        case .settingsAdvanced: return "settings/advanced"
        case .advancedBalance: return "settings/advanced/balance"
        case .advancedPeers: return "settings/advanced/peers"
        case .peersOpenChannel: return "settings/advanced/peers/open-channel"
        case .peersCloseChannel: return "settings/advanced/peers/close-channel"
        }
    }

    /// Concrete path with arguments substituted, PWA-style.
    var path: String {
        switch self {
        case let .activityCloseDetail(channelId): return "activity/close/\(channelId)"
        case let .activityTxDetail(txId): return "activity/\(txId)"
        default: return pattern
        }
    }

    /// Where "back" goes; `nil` on the tab-bar screens (Home/Activity).
    var backTo: Route? {
        switch self {
        case .home, .activity: return nil
        case .receive, .send, .scan, .recover, .settings: return .home
        case .activityCloseDetail, .activityTxDetail: return .activity
        case .settingsBackup, .settingsRestore, .settingsAdvanced: return .settings
        case .advancedBalance, .advancedPeers: return .settingsAdvanced
        case .peersOpenChannel, .peersCloseChannel: return .advancedPeers
        }
    }

    /// TabBar shows only on Home and Activity, like the PWA's `TAB_BAR_ROUTES`.
    var showsTabBar: Bool {
        self == .home || self == .activity
    }

    /// The NavigationStack path that puts this destination on screen: its
    /// backTo ancestors root-first, ending in self (Home is the stack root and
    /// never appears). Because the stack IS the backTo chain, the interactive
    /// pop gesture and the header arrow agree on where back lands (KTD-11).
    /// Activity has no backTo but is not the root; its pop lands on Home,
    /// matching Android's system-back mapping.
    var backChain: [Route] {
        var chain: [Route] = []
        var current: Route? = self
        while let route = current, route != .home {
            chain.append(route)
            current = route.backTo
        }
        return chain.reversed()
    }

    /// All 16 destinations (argument routes carry sample args), for tests and
    /// exhaustiveness checks — same order as Android's `Route.all`.
    static let all: [Route] = [
        .home, .receive, .send, .scan, .activity,
        .activityCloseDetail(channelId: "{channelId}"),
        .activityTxDetail(txId: "{txId}"),
        .recover, .settings, .settingsBackup, .settingsRestore, .settingsAdvanced,
        .advancedBalance, .advancedPeers, .peersOpenChannel, .peersCloseChannel,
    ]
}
