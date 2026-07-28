package zinqq.app

import uniffi.wallet_core.ActivityDirection
import uniffi.wallet_core.ActivityKind
import uniffi.wallet_core.ActivityRow
import uniffi.wallet_core.ActivityStatus
import uniffi.wallet_core.Balances
import uniffi.wallet_core.CloseInitiatorView
import uniffi.wallet_core.CloseRecordView
import uniffi.wallet_core.CloseStatusLabel
import uniffi.wallet_core.CloseTxRoleView
import uniffi.wallet_core.CloseTxView
import uniffi.wallet_core.CloseTypeView
import uniffi.wallet_core.PendingSweepView
import uniffi.wallet_core.RecoveryStateView
import uniffi.wallet_core.RecoveryStatusView

/**
 * Builders over the generated uniffi records so the derivation matrices
 * (U14) only spell out the fields each case exercises.
 */

fun activityRow(
    id: String = "row-id",
    kind: ActivityKind = ActivityKind.LIGHTNING,
    direction: ActivityDirection? = ActivityDirection.RECEIVED,
    amountMsat: ULong? = null,
    amountSats: ULong? = null,
    status: ActivityStatus = ActivityStatus.CONFIRMED,
    createdAtMs: ULong = 1_753_500_000_000uL,
    paymentHash: String? = null,
    txid: String? = null,
    channelId: String? = null,
    closeStatus: CloseStatusLabel? = null,
    failureReason: String? = null,
): ActivityRow = ActivityRow(
    id = id,
    kind = kind,
    direction = direction,
    amountMsat = amountMsat,
    amountSats = amountSats,
    status = status,
    createdAtMs = createdAtMs,
    paymentHash = paymentHash,
    txid = txid,
    channelId = channelId,
    closeStatus = closeStatus,
    failureReason = failureReason,
)

fun balancesFixture(
    lightningMsat: ULong = 0uL,
    onchainTotalSats: ULong = 0uL,
    onchainSpendableSats: ULong = 0uL,
    onchainUntrustedPendingSats: ULong = 0uL,
): Balances = Balances(
    lightningMsat = lightningMsat,
    onchainTotalSats = onchainTotalSats,
    onchainSpendableSats = onchainSpendableSats,
    onchainUntrustedPendingSats = onchainUntrustedPendingSats,
)

fun recoveryStateView(
    status: RecoveryStatusView = RecoveryStatusView.NEEDS_RECOVERY,
    stuckBalanceSat: ULong? = 50_000uL,
    depositAddress: String = "bc1qexampledepositaddressxxxxxxxxxxxxxxxxxx",
    depositNeededSat: ULong = 1_200uL,
    channelIds: List<String> = listOf("aa".repeat(32)),
): RecoveryStateView = RecoveryStateView(
    status = status,
    stuckBalanceSat = stuckBalanceSat,
    depositAddress = depositAddress,
    depositNeededSat = depositNeededSat,
    channelIds = channelIds,
    createdAtMs = 1_753_000_000_000uL,
    updatedAtMs = 1_753_000_000_000uL,
)

fun pendingSweepView(
    entryCount: UInt = 1u,
    descriptorCount: UInt = 1u,
    pendingSats: ULong = 5_000uL,
    hasUnknownValue: Boolean = false,
    lastAttemptFailed: Boolean = true,
    needsOnchainFunds: Boolean = false,
    shortfallSats: ULong? = null,
): PendingSweepView = PendingSweepView(
    entryCount = entryCount,
    descriptorCount = descriptorCount,
    pendingSats = pendingSats,
    hasUnknownValue = hasUnknownValue,
    lastAttemptFailed = lastAttemptFailed,
    needsOnchainFunds = needsOnchainFunds,
    shortfallSats = shortfallSats,
)

fun closeTxView(
    txid: String = "f0".repeat(32),
    role: CloseTxRoleView = CloseTxRoleView.COMMITMENT,
    feeSats: ULong? = null,
    confirmedAtHeight: UInt? = null,
    confirmations: UInt? = null,
): CloseTxView = CloseTxView(
    txid = txid,
    role = role,
    feeSats = feeSats,
    confirmedAtHeight = confirmedAtHeight,
    confirmations = confirmations,
)

fun closeRecordView(
    channelId: String = "aa".repeat(32),
    closeType: CloseTypeView = CloseTypeView.FORCE,
    initiator: CloseInitiatorView = CloseInitiatorView.REMOTE,
    closureReason: String? = null,
    status: CloseStatusLabel = CloseStatusLabel.WAITING_TIMELOCK,
    expectedAmountSats: ULong? = 5_000uL,
    timelockBlocks: UInt? = 144u,
    claimableAtHeight: UInt? = null,
    currentHeight: UInt? = null,
    createdAtMs: ULong = 1_753_500_000_000uL,
    completedAtMs: ULong? = null,
    resolvedUnverified: Boolean = false,
    txs: List<CloseTxView> = emptyList(),
): CloseRecordView = CloseRecordView(
    channelId = channelId,
    closeType = closeType,
    initiator = initiator,
    closureReason = closureReason,
    status = status,
    expectedAmountSats = expectedAmountSats,
    timelockBlocks = timelockBlocks,
    claimableAtHeight = claimableAtHeight,
    currentHeight = currentHeight,
    createdAtMs = createdAtMs,
    completedAtMs = completedAtMs,
    resolvedUnverified = resolvedUnverified,
    fundingTxid = null,
    fundingVout = null,
    txs = txs,
)
