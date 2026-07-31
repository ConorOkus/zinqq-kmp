package zinqq.app

import uniffi.wallet_core.WalletNetwork

/**
 * Maps the build's `BuildConfig.WALLET_NETWORK` string onto the core's network
 * enum (U6, R2).
 *
 * Kept pure and separate from [WalletHolder] so the build-type → network
 * decision is unit-testable — this module has no instrumentation-test
 * infrastructure, and "Release cannot end up on a test network" is exactly the
 * kind of guarantee that should be asserted rather than eyeballed.
 *
 * An unrecognized value resolves to mainnet. The Gradle side already falls back
 * to the build type's default, so reaching this branch means something is wrong
 * with the build config — and mainnet is the safe answer, since a wallet that
 * wrongly runs mainnet refuses test-network invoices, while one that wrongly
 * runs a test network could present a signet address as if it were real.
 */
fun walletNetworkFor(buildConfigValue: String): WalletNetwork =
    when (buildConfigValue.lowercase()) {
        "mutinynet" -> WalletNetwork.MUTINYNET
        else -> WalletNetwork.MAINNET
    }
