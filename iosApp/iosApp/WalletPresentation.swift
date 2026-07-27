import Foundation
import Shared

/// Pure display derivations for the wallet/activity screens (U19, R14):
/// every badge, bucket, label, and truncation the screens render is computed
/// here — screens only place the results. Ported helper-for-helper from
/// Android's `WalletPresentation.kt` with identical outputs; copy is verbatim
/// from the PWA (`Activity.tsx`, `TransactionDetail.tsx`,
/// `ChannelCloseDetail.tsx`, `RecoverFunds.tsx`, `RecoveryBanner.tsx`,
/// `PendingSweepBanner.tsx`).

/// The PWA's hard-coded explorer base (`TransactionDetail.tsx:7`).
let explorerTxBaseUrl = "https://mempool.space/tx"

func explorerTxUrl(_ txid: String) -> String { "\(explorerTxBaseUrl)/\(txid)" }

// MARK: - Activity list (Activity.tsx)

/// Relative-time buckets (`Activity.tsx:15-27`): empty for the zero sentinel
/// (an unconfirmed on-chain tx with no first-seen), "Just now" under a
/// minute, then floor-divided m/h/d/w buckets.
func formatRelativeTime(timestampMs: Int64, nowMs: Int64) -> String {
    if timestampMs == 0 { return "" }
    let seconds = (nowMs - timestampMs) / 1_000
    if seconds < 60 { return "Just now" }
    let minutes = seconds / 60
    if minutes < 60 { return "\(minutes)m ago" }
    let hours = minutes / 60
    if hours < 24 { return "\(hours)h ago" }
    let days = hours / 24
    if days < 7 { return "\(days)d ago" }
    return "\(days / 7)w ago"
}

/// Row title: "Channel close" / "Sent" / "Received" (`Activity.tsx`).
func activityTitle(_ row: ActivityRow) -> String {
    if row.kind == .channelClose { return "Channel close" }
    if row.direction == ActivityDirection.sent { return "Sent" }
    return "Received"
}

/// The muted badge next to the title: the PWA's `CLOSE_BADGES` table
/// (`Activity.tsx:7-13`) for close rows (empty-for-complete becomes nil),
/// "Pending" for pending payment rows, nothing otherwise.
func activityBadge(_ row: ActivityRow) -> String? {
    if row.kind == .channelClose {
        guard let closeStatus = row.closeStatus else { return nil }
        if closeStatus == .closing { return "Closing" }
        if closeStatus == .waitingTimelock { return "Waiting timelock" }
        if closeStatus == .returning { return "Returning to wallet" }
        if closeStatus == .resolvedUnverified { return "Resolved" }
        return nil // COMPLETE
    }
    return row.status == .pending ? "Pending" : nil
}

/// Display sats for a row: Lightning msat floored (never overstates), raw
/// sats otherwise; nil only for a close with an unknown return amount.
func rowAmountSats(_ row: ActivityRow) -> Int64? {
    if row.kind == .lightning {
        guard let msat = row.amountMsat else { return 0 }
        return FormatKt.msatToSatFloor(msat: msat.int64Value)
    }
    return row.amountSats?.int64Value
}

/// Signed amount text: `-₿X`/`+₿X` by direction; close rows are always `+₿X`
/// or an em-dash while the return amount is unknown (`Activity.tsx:77,113`).
func activityAmountText(_ row: ActivityRow) -> String {
    if row.kind == .channelClose {
        guard let sats = rowAmountSats(row) else { return "—" }
        return "+\(FormatKt.formatBtc(sats: sats))"
    }
    let sign = row.direction == ActivityDirection.sent ? "-" : "+"
    return "\(sign)\(FormatKt.formatBtc(sats: rowAmountSats(row) ?? 0))"
}

/// Pending rows render their amount muted (`Activity.tsx:74,110`).
func isAmountMuted(_ row: ActivityRow) -> Bool { row.status == .pending }

/// The `⚡` prefix shows on Lightning and close rows (`Activity.tsx:68,104`).
func showsLightningGlyph(_ row: ActivityRow) -> Bool { row.kind != .onchain }

// MARK: - Transaction detail (TransactionDetail.tsx)

/// `statusLabel` (`TransactionDetail.tsx:29-38`).
func txStatusLabel(_ status: ActivityStatus) -> String {
    if status == .confirmed { return "Complete" }
    if status == .pending { return "Pending" }
    return "Failed"
}

/// Fixed en-GB formatter: the PWA's `Intl.DateTimeFormat('en-GB', …)` output
/// must be byte-stable regardless of the device locale or its 12/24-hour
/// override (a fixed-identifier Locale ignores the user's hour-cycle
/// preference, unlike `Locale.current`).
private func enGbFormatter(_ pattern: String, _ timeZone: TimeZone) -> DateFormatter {
    let formatter = DateFormatter()
    formatter.locale = Locale(identifier: "en_GB")
    formatter.timeZone = timeZone
    formatter.dateFormat = pattern
    return formatter
}

/// `Intl.DateTimeFormat('en-GB', {weekday:'short', day:'numeric',
/// month:'long', year:'numeric'})` (`TransactionDetail.tsx:9-17`); the zero
/// sentinel renders "Pending".
func formatDetailDate(timestampMs: Int64, timeZone: TimeZone = .current) -> String {
    if timestampMs == 0 { return "Pending" }
    return enGbFormatter("EEE, d MMMM yyyy", timeZone)
        .string(from: Date(timeIntervalSince1970: Double(timestampMs) / 1_000))
}

/// 24-hour `HH:mm:ss` (`TransactionDetail.tsx:19-27`).
func formatDetailTime(timestampMs: Int64, timeZone: TimeZone = .current) -> String {
    if timestampMs == 0 { return "Pending" }
    return enGbFormatter("HH:mm:ss", timeZone)
        .string(from: Date(timeIntervalSince1970: Double(timestampMs) / 1_000))
}

/// Mid-truncation; each screen passes its own slice widths and ellipsis.
func midTruncate(_ value: String, head: Int, tail: Int, ellipsis: String = "...") -> String {
    if value.count <= head + tail + ellipsis.count { return value }
    return String(value.prefix(head)) + ellipsis + String(value.suffix(tail))
}

// MARK: - Channel close detail (ChannelCloseDetail.tsx)

/// `STATUS_LABELS` (`ChannelCloseDetail.tsx:16-22`).
func closeStatusLabel(_ status: CloseStatusLabel) -> String {
    if status == .closing { return "Closing" }
    if status == .waitingTimelock { return "Waiting (timelock)" }
    if status == .returning { return "Returning to wallet" }
    if status == .complete { return "Complete" }
    return "Resolved (unverified)"
}

/// `ROLE_LABELS` (`ChannelCloseDetail.tsx:24-30`); `Other` carries roles from
/// newer schema versions the PWA doesn't know either — neutral label.
func closeTxRoleLabel(_ role: CloseTxRoleView) -> String {
    if role == .closing { return "Closing transaction" }
    if role == .commitment { return "Commitment transaction" }
    if role == .feeBump { return "Fee bump (CPFP)" }
    if role == .paymentClaim { return "Payment claim" }
    if role == .sweepToWallet { return "Sweep to wallet" }
    return "Transaction"
}

/// Close type row (`ChannelCloseDetail.tsx:195-204`).
func closeTypeLabel(_ type: CloseTypeView) -> String {
    if type == .coop { return "Cooperative" }
    if type == .force { return "Force close" }
    return "Unknown"
}

func isTerminalClose(_ status: CloseStatusLabel) -> Bool {
    status == .complete || status == .resolvedUnverified
}

/// Hero amount: `~₿X` while the close is still resolving (the amount is an
/// estimate), bare `₿X` once terminal, em-dash while unknown — never a lying
/// zero (`ChannelCloseDetail.tsx:158-162`).
func closeAmountText(_ record: CloseRecordView) -> String {
    guard let sats = record.expectedAmountSats?.int64Value else { return "—" }
    let tilde = isTerminalClose(record.status) ? "" : "~"
    return "\(tilde)\(FormatKt.formatBtc(sats: sats))"
}

/// `claimableAtHeight - tip`, floored at 0; nil while either is unknown.
func blocksRemaining(_ record: CloseRecordView) -> Int64? {
    guard let claimable = record.claimableAtHeight?.int64Value else { return nil }
    guard let tip = record.currentHeight?.int64Value else { return nil }
    return max(0, claimable - tip)
}

/// `humanizeBlocks` (`close-records/estimate.ts:60-66`): 10 minutes a block;
/// minutes under an hour, rounded hours under 48, rounded days after.
func humanizeBlocks(_ blocks: Int64) -> String {
    let minutes = blocks * 10
    if minutes < 60 { return "~\(minutes) minutes" }
    let hours = Int64((Double(minutes) / 60).rounded())
    if hours < 48 { return "~\(hours) \(hours == 1 ? "hour" : "hours")" }
    return "~\(Int64((Double(hours) / 24).rounded())) days"
}

/// Total fees row: sum of the known per-tx fees (`ChannelCloseDetail.tsx:147`).
func totalFeesSats(_ record: CloseRecordView) -> Int64 {
    record.txs.reduce(0) { $0 + ($1.feeSats?.int64Value ?? 0) }
}

/// Needs-deposit is derived from RecoveryState at render — never stored on
/// the record (`ChannelCloseDetail.tsx:141-142`).
func needsDeposit(_ recovery: RecoveryStateView?, channelId: String) -> Bool {
    guard let recovery else { return false }
    return recovery.status == .needsRecovery && recovery.channelIds.contains(channelId)
}

/// Per-tx confirmation caption (`ChannelCloseDetail.tsx:73-77`): unconfirmed,
/// a live count when the core computed one, or a bare "Confirmed" when the
/// tip is unknown.
func confirmationText(_ tx: CloseTxView) -> String {
    if tx.confirmedAtHeight == nil { return "Unconfirmed" }
    guard let confs = tx.confirmations?.int64Value else { return "Confirmed" }
    return "\(confs) conf\(confs == 1 ? "" : "s")"
}

/// `Intl.DateTimeFormat('en-GB', {day:'numeric', month:'long',
/// year:'numeric', hour:'2-digit', minute:'2-digit'})`
/// (`ChannelCloseDetail.tsx:32-40`).
func formatCloseDate(timestampMs: Int64, timeZone: TimeZone = .current) -> String {
    enGbFormatter("d MMMM yyyy 'at' HH:mm", timeZone)
        .string(from: Date(timeIntervalSince1970: Double(timestampMs) / 1_000))
}

// MARK: - Home (Home.tsx, RecoveryBanner.tsx, PendingSweepBanner.tsx)

/// What Home's BalanceDisplay renders, per `use-unified-balance.ts`.
struct HomeBalance: Equatable {
    /// Full on-chain (confirmed + all pending) + floored Lightning sats.
    let totalSats: Int64
    /// The `+₿X pending` line: unconfirmed external receives only.
    let pendingSats: Int64
}

func homeBalance(_ balances: Balances) -> HomeBalance {
    HomeBalance(
        totalSats: FormatKt.msatToSatFloor(msat: Int64(bitPattern: balances.lightningMsat))
            + Int64(bitPattern: balances.onchainTotalSats),
        pendingSats: Int64(bitPattern: balances.onchainUntrustedPendingSats)
    )
}

/// One of `RecoveryBanner.tsx`'s two variants, fully derived.
struct RecoveryBannerUi: Equatable {
    let title: String
    let subtitle: String
    /// Needs-recovery taps through to the Recover screen.
    let navigatesToRecover: Bool
    /// Only the sweep-confirmed success banner can be dismissed.
    let dismissible: Bool
}

/// RecoveryBanner gating (`Home.tsx:80-84` + `RecoveryBanner.tsx`): shown
/// whenever recovery state exists; a dismissal only ever hides the
/// sweep-confirmed success variant (the needs-recovery banner has no dismiss
/// affordance).
func recoveryBanner(_ recovery: RecoveryStateView?, dismissed: Bool) -> RecoveryBannerUi? {
    guard let recovery else { return nil }
    if recovery.status == .sweepConfirmed {
        if dismissed { return nil }
        return RecoveryBannerUi(
            title: "Funds recovered!",
            subtitle: "Available in approximately 14 days",
            navigatesToRecover: false,
            dismissible: true
        )
    }
    return RecoveryBannerUi(
        title: "Your funds are safe",
        subtitle: "A small deposit is needed to unlock them",
        navigatesToRecover: true,
        dismissible: false
    )
}

/// The `PendingSweepBanner.tsx` heading/subtext/deep-link matrix, derived.
struct SweepBannerUi: Equatable {
    let heading: String
    let subtitle: String
    /// Needs-on-chain-funds taps through to Receive to add the deposit.
    let navigatesToReceive: Bool
}

/// PendingSweepBanner (`Home.tsx:86-90` + `PendingSweepBanner.tsx`): only
/// after a failed sweep attempt; `pendingSats` is a lower bound, so unknown
/// values get an "At least" prefix, and a shortfall is a floor phrased as
/// "at least".
func sweepBanner(_ info: PendingSweepView?) -> SweepBannerUi? {
    guard let info, info.lastAttemptFailed else { return nil }
    let amount = info.pendingSats > 0
        ? FormatKt.formatBtc(sats: Int64(bitPattern: info.pendingSats))
        : nil
    let heading: String
    if let amount {
        heading = info.hasUnknownValue
            ? "At least \(amount) waiting to sweep"
            : "\(amount) waiting to sweep"
    } else {
        heading = "Funds waiting to sweep"
    }
    if info.needsOnchainFunds {
        let subtitle: String
        if let shortfall = info.shortfallSats?.int64Value, shortfall > 0 {
            subtitle = "Add at least \(FormatKt.formatBtc(sats: shortfall)) "
                + "to cover network fees and recover these funds"
        } else {
            subtitle = "Add bitcoin to cover network fees and recover these funds"
        }
        return SweepBannerUi(heading: heading, subtitle: subtitle, navigatesToReceive: true)
    }
    return SweepBannerUi(
        heading: heading,
        subtitle: "Recovered funds return to your balance automatically when network fees allow",
        navigatesToReceive: false
    )
}
