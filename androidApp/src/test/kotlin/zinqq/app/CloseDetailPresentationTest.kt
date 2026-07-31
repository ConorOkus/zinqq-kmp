package zinqq.app

import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertFalse
import kotlin.test.assertNull
import kotlin.test.assertTrue
import uniffi.wallet_core.ActivityStatus
import uniffi.wallet_core.CloseStatusLabel
import uniffi.wallet_core.CloseTxRoleView
import uniffi.wallet_core.CloseTypeView
import uniffi.wallet_core.RecoveryStatusView

/**
 * Pure derivations for the transaction/channel-close detail screens,
 * mirroring `ChannelCloseDetail.tsx` (`STATUS_LABELS`, `ROLE_LABELS`,
 * needs-deposit, blocks remaining, total fees, txid truncation, conf counts)
 * and `TransactionDetail.tsx` (`statusLabel`, mid-truncated explorer link).
 */
class CloseDetailPresentationTest {
    @Test
    fun txStatusLabels() {
        assertEquals("Complete", txStatusLabel(ActivityStatus.CONFIRMED))
        assertEquals("Pending", txStatusLabel(ActivityStatus.PENDING))
        assertEquals("Failed", txStatusLabel(ActivityStatus.FAILED))
    }

    @Test
    fun closeStatusLabels() {
        assertEquals("Closing", closeStatusLabel(CloseStatusLabel.CLOSING))
        assertEquals("Waiting (timelock)", closeStatusLabel(CloseStatusLabel.WAITING_TIMELOCK))
        assertEquals("Returning to wallet", closeStatusLabel(CloseStatusLabel.RETURNING))
        assertEquals("Complete", closeStatusLabel(CloseStatusLabel.COMPLETE))
        assertEquals(
            "Resolved (unverified)",
            closeStatusLabel(CloseStatusLabel.RESOLVED_UNVERIFIED),
        )
    }

    @Test
    fun closeTxRoleLabels() {
        assertEquals("Closing transaction", closeTxRoleLabel(CloseTxRoleView.CLOSING))
        assertEquals("Commitment transaction", closeTxRoleLabel(CloseTxRoleView.COMMITMENT))
        assertEquals("Fee bump (CPFP)", closeTxRoleLabel(CloseTxRoleView.FEE_BUMP))
        assertEquals("Payment claim", closeTxRoleLabel(CloseTxRoleView.PAYMENT_CLAIM))
        assertEquals("Sweep to wallet", closeTxRoleLabel(CloseTxRoleView.SWEEP_TO_WALLET))
        // Forward-compat roles from newer schema versions get a neutral label.
        assertEquals("Transaction", closeTxRoleLabel(CloseTxRoleView.OTHER))
    }

    @Test
    fun closeTypeLabels() {
        assertEquals("Cooperative", closeTypeLabel(CloseTypeView.COOP))
        assertEquals("Force close", closeTypeLabel(CloseTypeView.FORCE))
        assertEquals("Unknown", closeTypeLabel(CloseTypeView.UNKNOWN))
    }

    @Test
    fun heroAmountGetsATildeWhileNonTerminalAndEmDashWhenUnknown() {
        assertEquals(
            "~₿5,000",
            closeAmountText(
                closeRecordView(status = CloseStatusLabel.WAITING_TIMELOCK),
            ),
        )
        assertEquals(
            "₿5,000",
            closeAmountText(closeRecordView(status = CloseStatusLabel.COMPLETE)),
        )
        assertEquals(
            "₿5,000",
            closeAmountText(closeRecordView(status = CloseStatusLabel.RESOLVED_UNVERIFIED)),
        )
        assertEquals(
            "—",
            closeAmountText(closeRecordView(expectedAmountSats = null)),
        )
    }

    @Test
    fun blocksRemainingCountsDownToZeroAndNeedsBothHeights() {
        assertEquals(
            20L,
            blocksRemaining(closeRecordView(claimableAtHeight = 900u, currentHeight = 880u)),
        )
        assertEquals(
            0L,
            blocksRemaining(closeRecordView(claimableAtHeight = 900u, currentHeight = 950u)),
        )
        assertNull(blocksRemaining(closeRecordView(claimableAtHeight = 900u, currentHeight = null)))
        assertNull(blocksRemaining(closeRecordView(claimableAtHeight = null, currentHeight = 880u)))
    }

    @Test
    fun totalFeesSumsSkippingUnknowns() {
        val record = closeRecordView(
            txs = listOf(
                closeTxView(feeSats = 300uL),
                closeTxView(feeSats = null),
                closeTxView(feeSats = 200uL),
            ),
        )
        assertEquals(500L, totalFeesSats(record))
    }

    @Test
    fun needsDepositRequiresNeedsRecoveryStatusNamingThisChannel() {
        val channelId = "aa".repeat(32)
        assertTrue(
            needsDeposit(
                recoveryStateView(
                    status = RecoveryStatusView.NEEDS_RECOVERY,
                    channelIds = listOf(channelId),
                ),
                channelId,
            ),
        )
        assertFalse(
            needsDeposit(
                recoveryStateView(
                    status = RecoveryStatusView.SWEEP_CONFIRMED,
                    channelIds = listOf(channelId),
                ),
                channelId,
            ),
        )
        assertFalse(
            needsDeposit(
                recoveryStateView(
                    status = RecoveryStatusView.NEEDS_RECOVERY,
                    channelIds = listOf("bb".repeat(32)),
                ),
                channelId,
            ),
        )
        assertFalse(needsDeposit(null, channelId))
    }

    @Test
    fun confirmationTextPrefersTheCoreCountAndMarksUnconfirmed() {
        assertEquals("Unconfirmed", confirmationText(closeTxView(confirmedAtHeight = null)))
        assertEquals(
            "1 conf",
            confirmationText(closeTxView(confirmedAtHeight = 900u, confirmations = 1u)),
        )
        assertEquals(
            "3 confs",
            confirmationText(closeTxView(confirmedAtHeight = 900u, confirmations = 3u)),
        )
        // Confirmed at a height but no tip to count against.
        assertEquals(
            "Confirmed",
            confirmationText(closeTxView(confirmedAtHeight = 900u, confirmations = null)),
        )
    }

    @Test
    fun midTruncationMatchesEachScreensSliceWidths() {
        val txid = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
        // TransactionDetail.tsx:127 — 8/8 with three dots.
        assertEquals("01234567...89abcdef", midTruncate(txid, 8, 8, "..."))
        // ChannelCloseDetail.tsx:87 — 10/10 with an ellipsis char.
        assertEquals("0123456789…6789abcdef", midTruncate(txid, 10, 10, "…"))
        // RecoverFunds.tsx:35-36 — 12/8 with three dots.
        val address = "bc1qw508d6qejxtdg4y5r3zarvary0c5xw7kv8f3t4"
        assertEquals("bc1qw508d6qe...7kv8f3t4", midTruncate(address, 12, 8, "..."))
    }

    @Test
    fun explorerLinksPointAtMempoolSpace() {
        assertEquals(
            "https://mempool.space/tx/abc123",
            explorerTxUrl("https://mempool.space", "abc123"),
        )
        // R8: the link follows the build's network, so a Mutinynet build does
        // not send a signet txid to a mainnet explorer.
        assertEquals(
            "https://mutinynet.com/tx/abc123",
            explorerTxUrl("https://mutinynet.com", "abc123"),
        )
        // A trailing slash on the configured base must not double up.
        assertEquals(
            "https://mempool.space/tx/abc123",
            explorerTxUrl("https://mempool.space/", "abc123"),
        )
    }
}
