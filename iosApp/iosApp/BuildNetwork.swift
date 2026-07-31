import Shared

/// Maps the build's `WalletNetwork` Info.plist value onto the core's network
/// enum (U7, R2). The value comes from the per-configuration `WALLET_NETWORK`
/// build setting in `project.yml` — Debug is Mutinynet, Release is mainnet.
///
/// Pure and separate from `WalletModel` so the configuration → network decision
/// is unit-testable; "a shipped build is on mainnet" is worth asserting rather
/// than eyeballing.
///
/// An unrecognized or missing value resolves to mainnet. A wallet wrongly on
/// mainnet refuses test-network invoices, while one wrongly on a test network
/// could present a worthless address as if it were real — so the safe failure
/// direction is mainnet.
func walletNetworkFor(_ infoPlistValue: String?) -> WalletNetwork {
    switch infoPlistValue?.lowercased() {
    case "mutinynet": return .mutinynet
    default: return .mainnet
    }
}

/// The network this build was compiled for, read from the bundle.
func buildWalletNetwork() -> WalletNetwork {
    walletNetworkFor(
        Bundle.main.object(forInfoDictionaryKey: "WalletNetwork") as? String
    )
}
