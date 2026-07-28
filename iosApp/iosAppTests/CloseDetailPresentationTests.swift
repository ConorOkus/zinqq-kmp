import XCTest

@testable import iosApp

/// Pure derivations for the transaction/channel-close detail screens,
/// mirroring `ChannelCloseDetail.tsx` (`STATUS_LABELS`, `ROLE_LABELS`,
/// needs-deposit, blocks remaining, total fees, txid truncation, conf counts)
/// and `TransactionDetail.tsx` (`statusLabel`, mid-truncated explorer link) —
/// the same matrix as Android's `CloseDetailPresentationTest`.
final class CloseDetailPresentationTests: XCTestCase {
    func testTxStatusLabels() {
        XCTAssertEqual("Complete", txStatusLabel(.confirmed))
        XCTAssertEqual("Pending", txStatusLabel(.pending))
        XCTAssertEqual("Failed", txStatusLabel(.failed))
    }

    func testCloseStatusLabels() {
        XCTAssertEqual("Closing", closeStatusLabel(.closing))
        XCTAssertEqual("Waiting (timelock)", closeStatusLabel(.waitingTimelock))
        XCTAssertEqual("Returning to wallet", closeStatusLabel(.returning))
        XCTAssertEqual("Complete", closeStatusLabel(.complete))
        XCTAssertEqual("Resolved (unverified)", closeStatusLabel(.resolvedUnverified))
    }

    func testCloseTxRoleLabels() {
        XCTAssertEqual("Closing transaction", closeTxRoleLabel(.closing))
        XCTAssertEqual("Commitment transaction", closeTxRoleLabel(.commitment))
        XCTAssertEqual("Fee bump (CPFP)", closeTxRoleLabel(.feeBump))
        XCTAssertEqual("Payment claim", closeTxRoleLabel(.paymentClaim))
        XCTAssertEqual("Sweep to wallet", closeTxRoleLabel(.sweepToWallet))
        // Forward-compat roles from newer schema versions get a neutral label.
        XCTAssertEqual("Transaction", closeTxRoleLabel(.other))
    }

    func testCloseTypeLabels() {
        XCTAssertEqual("Cooperative", closeTypeLabel(.coop))
        XCTAssertEqual("Force close", closeTypeLabel(.force))
        XCTAssertEqual("Unknown", closeTypeLabel(.unknown))
    }

    func testHeroAmountGetsATildeWhileNonTerminalAndEmDashWhenUnknown() {
        XCTAssertEqual(
            "~₿5,000",
            closeAmountText(closeRecordView(status: .waitingTimelock))
        )
        XCTAssertEqual(
            "₿5,000",
            closeAmountText(closeRecordView(status: .complete))
        )
        XCTAssertEqual(
            "₿5,000",
            closeAmountText(closeRecordView(status: .resolvedUnverified))
        )
        XCTAssertEqual(
            "—",
            closeAmountText(closeRecordView(expectedAmountSats: nil))
        )
    }

    func testBlocksRemainingCountsDownToZeroAndNeedsBothHeights() {
        XCTAssertEqual(
            20,
            blocksRemaining(closeRecordView(claimableAtHeight: 900, currentHeight: 880))
        )
        XCTAssertEqual(
            0,
            blocksRemaining(closeRecordView(claimableAtHeight: 900, currentHeight: 950))
        )
        XCTAssertNil(
            blocksRemaining(closeRecordView(claimableAtHeight: 900, currentHeight: nil))
        )
        XCTAssertNil(
            blocksRemaining(closeRecordView(claimableAtHeight: nil, currentHeight: 880))
        )
    }

    func testTotalFeesSumsSkippingUnknowns() {
        let record = closeRecordView(
            txs: [
                closeTxView(feeSats: 300),
                closeTxView(feeSats: nil),
                closeTxView(feeSats: 200),
            ]
        )
        XCTAssertEqual(500, totalFeesSats(record))
    }

    func testNeedsDepositRequiresNeedsRecoveryStatusNamingThisChannel() {
        let channelId = String(repeating: "aa", count: 32)
        XCTAssertTrue(
            needsDeposit(
                recoveryStateView(status: .needsRecovery, channelIds: [channelId]),
                channelId: channelId
            )
        )
        XCTAssertFalse(
            needsDeposit(
                recoveryStateView(status: .sweepConfirmed, channelIds: [channelId]),
                channelId: channelId
            )
        )
        XCTAssertFalse(
            needsDeposit(
                recoveryStateView(
                    status: .needsRecovery,
                    channelIds: [String(repeating: "bb", count: 32)]
                ),
                channelId: channelId
            )
        )
        XCTAssertFalse(needsDeposit(nil, channelId: channelId))
    }

    func testConfirmationTextPrefersTheCoreCountAndMarksUnconfirmed() {
        XCTAssertEqual("Unconfirmed", confirmationText(closeTxView(confirmedAtHeight: nil)))
        XCTAssertEqual(
            "1 conf",
            confirmationText(closeTxView(confirmedAtHeight: 900, confirmations: 1))
        )
        XCTAssertEqual(
            "3 confs",
            confirmationText(closeTxView(confirmedAtHeight: 900, confirmations: 3))
        )
        // Confirmed at a height but no tip to count against.
        XCTAssertEqual(
            "Confirmed",
            confirmationText(closeTxView(confirmedAtHeight: 900, confirmations: nil))
        )
    }

    func testMidTruncationMatchesEachScreensSliceWidths() {
        let txid = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
        // TransactionDetail.tsx:127 — 8/8 with three dots.
        XCTAssertEqual("01234567...89abcdef", midTruncate(txid, head: 8, tail: 8, ellipsis: "..."))
        // ChannelCloseDetail.tsx:87 — 10/10 with an ellipsis char.
        XCTAssertEqual(
            "0123456789…6789abcdef",
            midTruncate(txid, head: 10, tail: 10, ellipsis: "…")
        )
        // RecoverFunds.tsx:35-36 — 12/8 with three dots.
        let address = "bc1qw508d6qejxtdg4y5r3zarvary0c5xw7kv8f3t4"
        XCTAssertEqual(
            "bc1qw508d6qe...7kv8f3t4",
            midTruncate(address, head: 12, tail: 8, ellipsis: "...")
        )
    }

    func testExplorerLinksPointAtMempoolSpace() {
        XCTAssertEqual("https://mempool.space/tx/abc123", explorerTxUrl("abc123"))
    }
}
