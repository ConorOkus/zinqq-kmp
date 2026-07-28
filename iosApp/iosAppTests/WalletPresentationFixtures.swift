import Shared

@testable import iosApp

/// Builders over the generated uniffi records so the derivation matrices
/// (U19) only spell out the fields each case exercises — the same fixtures as
/// Android's `Fixtures.kt`.

func activityRow(
    id: String = "row-id",
    kind: ActivityKind = .lightning,
    direction: ActivityDirection? = .received,
    amountMsat: UInt64? = nil,
    amountSats: UInt64? = nil,
    status: ActivityStatus = .confirmed,
    createdAtMs: UInt64 = 1_753_500_000_000,
    paymentHash: String? = nil,
    txid: String? = nil,
    channelId: String? = nil,
    closeStatus: CloseStatusLabel? = nil,
    failureReason: String? = nil
) -> ActivityRow {
    ActivityRow(
        id: id,
        kind: kind,
        direction: direction,
        amountMsat: amountMsat.map { KotlinULong(unsignedLongLong: $0) },
        amountSats: amountSats.map { KotlinULong(unsignedLongLong: $0) },
        status: status,
        createdAtMs: createdAtMs,
        paymentHash: paymentHash,
        txid: txid,
        channelId: channelId,
        closeStatus: closeStatus,
        failureReason: failureReason
    )
}

func balancesFixture(
    lightningMsat: UInt64 = 0,
    onchainTotalSats: UInt64 = 0,
    onchainSpendableSats: UInt64 = 0,
    onchainUntrustedPendingSats: UInt64 = 0
) -> Balances {
    Balances(
        lightningMsat: lightningMsat,
        onchainTotalSats: onchainTotalSats,
        onchainSpendableSats: onchainSpendableSats,
        onchainUntrustedPendingSats: onchainUntrustedPendingSats
    )
}

func recoveryStateView(
    status: RecoveryStatusView = .needsRecovery,
    stuckBalanceSat: UInt64? = 50_000,
    depositAddress: String = "bc1qexampledepositaddressxxxxxxxxxxxxxxxxxx",
    depositNeededSat: UInt64 = 1_200,
    channelIds: [String] = [String(repeating: "aa", count: 32)]
) -> RecoveryStateView {
    RecoveryStateView(
        status: status,
        stuckBalanceSat: stuckBalanceSat.map { KotlinULong(unsignedLongLong: $0) },
        depositAddress: depositAddress,
        depositNeededSat: depositNeededSat,
        channelIds: channelIds,
        createdAtMs: 1_753_000_000_000,
        updatedAtMs: 1_753_000_000_000
    )
}

func pendingSweepView(
    entryCount: UInt32 = 1,
    descriptorCount: UInt32 = 1,
    pendingSats: UInt64 = 5_000,
    hasUnknownValue: Bool = false,
    lastAttemptFailed: Bool = true,
    needsOnchainFunds: Bool = false,
    shortfallSats: UInt64? = nil
) -> PendingSweepView {
    PendingSweepView(
        entryCount: entryCount,
        descriptorCount: descriptorCount,
        pendingSats: pendingSats,
        hasUnknownValue: hasUnknownValue,
        lastAttemptFailed: lastAttemptFailed,
        needsOnchainFunds: needsOnchainFunds,
        shortfallSats: shortfallSats.map { KotlinULong(unsignedLongLong: $0) }
    )
}

func closeTxView(
    txid: String = String(repeating: "f0", count: 32),
    role: CloseTxRoleView = .commitment,
    feeSats: UInt64? = nil,
    confirmedAtHeight: UInt32? = nil,
    confirmations: UInt32? = nil
) -> CloseTxView {
    CloseTxView(
        txid: txid,
        role: role,
        feeSats: feeSats.map { KotlinULong(unsignedLongLong: $0) },
        confirmedAtHeight: confirmedAtHeight.map { KotlinUInt(unsignedInt: $0) },
        confirmations: confirmations.map { KotlinUInt(unsignedInt: $0) }
    )
}

func closeRecordView(
    channelId: String = String(repeating: "aa", count: 32),
    closeType: CloseTypeView = .force,
    initiator: CloseInitiatorView = .remote,
    closureReason: String? = nil,
    status: CloseStatusLabel = .waitingTimelock,
    expectedAmountSats: UInt64? = 5_000,
    timelockBlocks: UInt32? = 144,
    claimableAtHeight: UInt32? = nil,
    currentHeight: UInt32? = nil,
    createdAtMs: UInt64 = 1_753_500_000_000,
    completedAtMs: UInt64? = nil,
    resolvedUnverified: Bool = false,
    txs: [CloseTxView] = []
) -> CloseRecordView {
    CloseRecordView(
        channelId: channelId,
        closeType: closeType,
        initiator: initiator,
        closureReason: closureReason,
        status: status,
        expectedAmountSats: expectedAmountSats.map { KotlinULong(unsignedLongLong: $0) },
        timelockBlocks: timelockBlocks.map { KotlinUInt(unsignedInt: $0) },
        claimableAtHeight: claimableAtHeight.map { KotlinUInt(unsignedInt: $0) },
        currentHeight: currentHeight.map { KotlinUInt(unsignedInt: $0) },
        createdAtMs: createdAtMs,
        completedAtMs: completedAtMs.map { KotlinULong(unsignedLongLong: $0) },
        resolvedUnverified: resolvedUnverified,
        fundingTxid: nil,
        fundingVout: nil,
        txs: txs
    )
}
