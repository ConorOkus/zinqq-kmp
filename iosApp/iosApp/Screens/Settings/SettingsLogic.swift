import Foundation
import Shared

/// Pure derivations for the Settings, Advanced, and Balance screens (U22,
/// R12, R14): the PWA's row tables, the appearance radiogroup mapping, and
/// the balance breakdown — screens only place the results. Ported
/// value-for-value from Android's `SettingsLogic.kt`; copy is verbatim from
/// `Settings.tsx`, `Advanced.tsx`, and `Balance.tsx`.

/// One tappable settings row; `destination` nil = inert no-op (PWA parity).
struct SettingsRowSpec: Equatable {
    let label: String
    let detail: String
    let destination: Route?
}

/// `SETTINGS_ITEMS` (`Settings.tsx:12-107`). How It Works and Get Help ship
/// as no-ops in the PWA (`route: null`) — replicated as inert rows (plan
/// Scope Boundaries).
let settingsRows: [SettingsRowSpec] = [
    SettingsRowSpec(label: "Wallet Backup", detail: "Setup", destination: .settingsBackup),
    SettingsRowSpec(label: "Recover Wallet", detail: "From Seed", destination: .settingsRestore),
    SettingsRowSpec(label: "Advanced", detail: "Settings", destination: .settingsAdvanced),
    SettingsRowSpec(label: "How It Works", detail: "FAQ", destination: nil),
    SettingsRowSpec(label: "Get Help", detail: "Chat with us", destination: nil),
]

/// `ADVANCED_ITEMS` (`Advanced.tsx:6-47`).
let advancedRows: [SettingsRowSpec] = [
    SettingsRowSpec(label: "Balance", detail: "Onchain · Lightning", destination: .advancedBalance),
    SettingsRowSpec(label: "Peers", detail: "Connected", destination: .advancedPeers),
]

/// The radiogroup's order: `THEME_MODES` (`theme.ts:3`) — hybrid, light, dark.
let appearanceModes: [AppearanceMode] = [.hybrid, .light, .dark]

/// `THEME_LABELS` (`Settings.tsx:6-10`).
func appearanceLabel(_ mode: AppearanceMode) -> String {
    switch mode {
    case .hybrid: return "Hybrid"
    case .light: return "Light"
    case .dark: return "Dark"
    }
}

/// What the Balance screen renders, per `use-unified-balance.ts`.
struct BalanceBreakdown: Equatable {
    /// Full on-chain (confirmed + all pending) + floored Lightning sats.
    let totalSats: Int64
    /// The `+₿X pending` line: unconfirmed external receives only.
    let pendingSats: Int64
    let onchainSats: Int64
    let lightningSats: Int64
}

func balanceBreakdown(_ balances: Balances) -> BalanceBreakdown {
    let onchain = Int64(bitPattern: balances.onchainTotalSats)
    let lightning = FormatKt.msatToSatFloor(msat: Int64(bitPattern: balances.lightningMsat))
    return BalanceBreakdown(
        totalSats: onchain + lightning,
        pendingSats: Int64(bitPattern: balances.onchainUntrustedPendingSats),
        onchainSats: onchain,
        lightningSats: lightning
    )
}
