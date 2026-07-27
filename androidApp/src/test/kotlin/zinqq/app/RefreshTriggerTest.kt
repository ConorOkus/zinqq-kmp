package zinqq.app

import kotlin.test.Test
import kotlin.test.assertFalse
import kotlin.test.assertTrue
import uniffi.wallet_core.Event

/**
 * The event set that re-queries wallet data (balances, activity, recovery,
 * pending sweep): the spike's balance triggers extended with the sweep and
 * recovery change events (U14; the PWA's `usePendingSweep`/`useRecovery`
 * re-read on their change notifications).
 */
class RefreshTriggerTest {
    @Test
    fun settlementAndStateChangeEventsTriggerARefresh() {
        assertTrue(
            shouldRefreshWalletData(
                Event.PaymentReceived(
                    paymentHash = "11".repeat(32),
                    amountMsat = 1_000uL,
                    skimmedFeeMsat = null,
                ),
            ),
        )
        assertTrue(
            shouldRefreshWalletData(
                Event.PaymentSuccessful(paymentHash = "22".repeat(32), feePaidMsat = null),
            ),
        )
        assertTrue(shouldRefreshWalletData(Event.ChannelReady(channelId = "33".repeat(32))))
        assertTrue(shouldRefreshWalletData(Event.SweepStateChanged))
        assertTrue(shouldRefreshWalletData(Event.RecoveryStateChanged))
    }

    @Test
    fun otherEventsDoNot() {
        assertFalse(shouldRefreshWalletData(Event.NodeStarted))
        assertFalse(shouldRefreshWalletData(Event.SyncCompleted))
        assertFalse(shouldRefreshWalletData(Event.SyncFailed))
        assertFalse(
            shouldRefreshWalletData(
                Event.InvoiceReady(bolt11 = "lnbc1", expiryUnixSecs = 0uL),
            ),
        )
        assertFalse(
            shouldRefreshWalletData(Event.ChannelPending(channelId = "44".repeat(32))),
        )
    }
}
