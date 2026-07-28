package zinqq.app

import kotlin.test.Test
import kotlin.test.assertEquals

/**
 * Home balance mapping, mirroring the PWA's `useUnifiedBalance`
 * (`use-unified-balance.ts`): total = full on-chain (confirmed + all
 * pending) + floored Lightning sats; the "+₿X pending" line is exactly the
 * untrusted pending (unconfirmed external receives).
 */
class BalanceMappingTest {
    @Test
    fun totalIsOnchainTotalPlusFlooredLightning() {
        val balance = homeBalance(
            balancesFixture(
                lightningMsat = 2_500_999uL,
                onchainTotalSats = 10_000uL,
                onchainSpendableSats = 8_000uL,
                onchainUntrustedPendingSats = 2_000uL,
            ),
        )
        assertEquals(12_500L, balance.totalSats)
    }

    @Test
    fun pendingLineIsUntrustedPendingOnly() {
        val balance = homeBalance(
            balancesFixture(
                lightningMsat = 1_000_000uL,
                onchainTotalSats = 10_000uL,
                onchainSpendableSats = 8_000uL,
                onchainUntrustedPendingSats = 2_000uL,
            ),
        )
        assertEquals(2_000L, balance.pendingSats)
    }

    @Test
    fun zeroBalancesMapToZero() {
        val balance = homeBalance(balancesFixture())
        assertEquals(0L, balance.totalSats)
        assertEquals(0L, balance.pendingSats)
    }
}
