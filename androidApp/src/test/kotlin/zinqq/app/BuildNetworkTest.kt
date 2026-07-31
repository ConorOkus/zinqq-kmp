package zinqq.app

import kotlin.test.Test
import kotlin.test.assertEquals
import uniffi.wallet_core.WalletNetwork

/**
 * The build-type → network mapping (U6, R2, AE6). These assertions stand in for
 * the guarantee TestFlight rests on: a shipped binary is on mainnet.
 */
class BuildNetworkTest {

    @Test
    fun aDebugBuildTargetsMutinynet() {
        assertEquals(WalletNetwork.MUTINYNET, walletNetworkFor("mutinynet"))
    }

    @Test
    fun theMainnetOverrideIsHonoured() {
        assertEquals(WalletNetwork.MAINNET, walletNetworkFor("mainnet"))
    }

    /**
     * AE6, at the shell layer: Release compiles "mainnet" in and reads no
     * property, so the only way a shipped build could reach a test network is
     * this mapping mishandling the value.
     */
    @Test
    fun theReleaseValueResolvesToMainnet() {
        assertEquals(WalletNetwork.MAINNET, walletNetworkFor("mainnet"))
    }

    /**
     * A broken build config must fail toward mainnet, never toward a test
     * network: a wallet wrongly on mainnet refuses signet invoices, while one
     * wrongly on signet could show a worthless address as if it were real.
     */
    @Test
    fun anUnrecognizedValueFallsBackToMainnet() {
        assertEquals(WalletNetwork.MAINNET, walletNetworkFor(""))
        assertEquals(WalletNetwork.MAINNET, walletNetworkFor("regtest"))
        assertEquals(WalletNetwork.MAINNET, walletNetworkFor("MUTINY"))
    }

    /** Case is not load-bearing — the Gradle side lowercases, this one too. */
    @Test
    fun theMappingIsCaseInsensitive() {
        assertEquals(WalletNetwork.MUTINYNET, walletNetworkFor("MutinyNet"))
        assertEquals(WalletNetwork.MAINNET, walletNetworkFor("MAINNET"))
    }
}
