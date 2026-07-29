package zinqq.app

import java.time.Instant
import java.time.ZoneId
import java.time.format.DateTimeFormatter
import java.util.Locale
import kotlin.math.roundToLong
import uniffi.wallet_core.ActivityDirection
import uniffi.wallet_core.ActivityKind
import uniffi.wallet_core.ActivityRow
import uniffi.wallet_core.ActivityStatus
import uniffi.wallet_core.Balances
import uniffi.wallet_core.CloseRecordView
import uniffi.wallet_core.CloseStatusLabel
import uniffi.wallet_core.CloseTxRoleView
import uniffi.wallet_core.CloseTxView
import uniffi.wallet_core.CloseTypeView
import uniffi.wallet_core.PendingSweepView
import uniffi.wallet_core.RecoveryStateView
import uniffi.wallet_core.RecoveryStatusView
import zinqq.main.formatBtc
import zinqq.main.msatToSatFloor

/**
 * Pure display derivations for the wallet/activity screens (U14, R14):
 * every badge, bucket, label, and truncation the screens render is computed
 * here — screens only place the results. Copy is verbatim from the PWA
 * (`Activity.tsx`, `TransactionDetail.tsx`, `ChannelCloseDetail.tsx`,
 * `RecoverFunds.tsx`, `RecoveryBanner.tsx`, `PendingSweepBanner.tsx`).
 */

/** The PWA's hard-coded explorer base (`TransactionDetail.tsx:7`). */
const val EXPLORER_TX_URL = "https://mempool.space/tx"

fun explorerTxUrl(txid: String): String = "$EXPLORER_TX_URL/$txid"

// ---------------------------------------------------------------------------
// Activity list (Activity.tsx)
// ---------------------------------------------------------------------------

/**
 * Relative-time buckets (`Activity.tsx:15-27`): empty for the zero sentinel
 * (an unconfirmed on-chain tx with no first-seen), "Just now" under a
 * minute, then floor-divided m/h/d/w buckets.
 */
fun formatRelativeTime(timestampMs: Long, nowMs: Long): String {
    if (timestampMs == 0L) return ""
    val seconds = (nowMs - timestampMs) / 1_000
    if (seconds < 60) return "Just now"
    val minutes = seconds / 60
    if (minutes < 60) return "${minutes}m ago"
    val hours = minutes / 60
    if (hours < 24) return "${hours}h ago"
    val days = hours / 24
    if (days < 7) return "${days}d ago"
    return "${days / 7}w ago"
}

/** Row title: "Channel close" / "Sent" / "Received" (`Activity.tsx`). */
fun activityTitle(row: ActivityRow): String = when {
    row.kind == ActivityKind.CHANNEL_CLOSE -> "Channel close"
    row.direction == ActivityDirection.SENT -> "Sent"
    else -> "Received"
}

/**
 * The muted badge next to the title: the PWA's `CLOSE_BADGES` table
 * (`Activity.tsx:7-13`) for close rows (empty-for-complete becomes null),
 * "Pending" for pending payment rows, nothing otherwise.
 */
fun activityBadge(row: ActivityRow): String? {
    if (row.kind == ActivityKind.CHANNEL_CLOSE) {
        return when (row.closeStatus) {
            CloseStatusLabel.CLOSING -> "Closing"
            CloseStatusLabel.WAITING_TIMELOCK -> "Waiting timelock"
            CloseStatusLabel.RETURNING -> "Returning to wallet"
            CloseStatusLabel.RESOLVED_UNVERIFIED -> "Resolved"
            CloseStatusLabel.COMPLETE, null -> null
        }
    }
    return if (row.status == ActivityStatus.PENDING) "Pending" else null
}

/**
 * Display sats for a row: Lightning msat floored (never overstates), raw
 * sats otherwise; null only for a close with an unknown return amount.
 */
fun rowAmountSats(row: ActivityRow): Long? = when (row.kind) {
    ActivityKind.LIGHTNING -> row.amountMsat?.let { msatToSatFloor(it.toLong()) } ?: 0L
    else -> row.amountSats?.toLong()
}

/**
 * Signed amount text: `-₿X`/`+₿X` by direction; close rows are always `+₿X`
 * or an em-dash while the return amount is unknown (`Activity.tsx:77,113`).
 */
fun activityAmountText(row: ActivityRow): String {
    if (row.kind == ActivityKind.CHANNEL_CLOSE) {
        val sats = rowAmountSats(row) ?: return "—"
        return "+${formatBtc(sats)}"
    }
    val sign = if (row.direction == ActivityDirection.SENT) "-" else "+"
    return "$sign${formatBtc(rowAmountSats(row) ?: 0L)}"
}

/** Pending rows render their amount muted (`Activity.tsx:74,110`). */
fun isAmountMuted(row: ActivityRow): Boolean = row.status == ActivityStatus.PENDING

/** The `⚡` prefix shows on Lightning and close rows (`Activity.tsx:68,104`). */
fun showsLightningGlyph(row: ActivityRow): Boolean = row.kind != ActivityKind.ONCHAIN

// ---------------------------------------------------------------------------
// Transaction detail (TransactionDetail.tsx)
// ---------------------------------------------------------------------------

/** `statusLabel` (`TransactionDetail.tsx:29-38`). */
fun txStatusLabel(status: ActivityStatus): String = when (status) {
    ActivityStatus.CONFIRMED -> "Complete"
    ActivityStatus.PENDING -> "Pending"
    ActivityStatus.FAILED -> "Failed"
}

/**
 * `Intl.DateTimeFormat('en-GB', {weekday:'short', day:'numeric',
 * month:'long', year:'numeric'})` (`TransactionDetail.tsx:9-17`); the zero
 * sentinel renders "Pending".
 */
fun formatDetailDate(timestampMs: Long, zone: ZoneId = ZoneId.systemDefault()): String {
    if (timestampMs == 0L) return "Pending"
    return DateTimeFormatter.ofPattern("EEE, d MMMM yyyy", Locale.UK)
        .format(Instant.ofEpochMilli(timestampMs).atZone(zone))
}

/** 24-hour `HH:mm:ss` (`TransactionDetail.tsx:19-27`). */
fun formatDetailTime(timestampMs: Long, zone: ZoneId = ZoneId.systemDefault()): String {
    if (timestampMs == 0L) return "Pending"
    return DateTimeFormatter.ofPattern("HH:mm:ss", Locale.UK)
        .format(Instant.ofEpochMilli(timestampMs).atZone(zone))
}

/** Mid-truncation; each screen passes its own slice widths and ellipsis. */
fun midTruncate(value: String, head: Int, tail: Int, ellipsis: String = "..."): String {
    if (value.length <= head + tail + ellipsis.length) return value
    return value.take(head) + ellipsis + value.takeLast(tail)
}

// ---------------------------------------------------------------------------
// Channel close detail (ChannelCloseDetail.tsx)
// ---------------------------------------------------------------------------

/** `STATUS_LABELS` (`ChannelCloseDetail.tsx:16-22`). */
fun closeStatusLabel(status: CloseStatusLabel): String = when (status) {
    CloseStatusLabel.CLOSING -> "Closing"
    CloseStatusLabel.WAITING_TIMELOCK -> "Waiting (timelock)"
    CloseStatusLabel.RETURNING -> "Returning to wallet"
    CloseStatusLabel.COMPLETE -> "Complete"
    CloseStatusLabel.RESOLVED_UNVERIFIED -> "Resolved (unverified)"
}

/**
 * `ROLE_LABELS` (`ChannelCloseDetail.tsx:24-30`); `Other` carries roles from
 * newer schema versions the PWA doesn't know either — neutral label.
 */
fun closeTxRoleLabel(role: CloseTxRoleView): String = when (role) {
    CloseTxRoleView.CLOSING -> "Closing transaction"
    CloseTxRoleView.COMMITMENT -> "Commitment transaction"
    CloseTxRoleView.FEE_BUMP -> "Fee bump (CPFP)"
    CloseTxRoleView.PAYMENT_CLAIM -> "Payment claim"
    CloseTxRoleView.SWEEP_TO_WALLET -> "Sweep to wallet"
    CloseTxRoleView.OTHER -> "Transaction"
}

/** Close type row (`ChannelCloseDetail.tsx:195-204`). */
fun closeTypeLabel(type: CloseTypeView): String = when (type) {
    CloseTypeView.COOP -> "Cooperative"
    CloseTypeView.FORCE -> "Force close"
    CloseTypeView.UNKNOWN -> "Unknown"
}

fun isTerminalClose(status: CloseStatusLabel): Boolean =
    status == CloseStatusLabel.COMPLETE || status == CloseStatusLabel.RESOLVED_UNVERIFIED

/**
 * Hero amount: `~₿X` while the close is still resolving (the amount is an
 * estimate), bare `₿X` once terminal, em-dash while unknown — never a lying
 * zero (`ChannelCloseDetail.tsx:158-162`).
 */
fun closeAmountText(record: CloseRecordView): String {
    val sats = record.expectedAmountSats?.toLong() ?: return "—"
    val tilde = if (isTerminalClose(record.status)) "" else "~"
    return "$tilde${formatBtc(sats)}"
}

/** `claimableAtHeight - tip`, floored at 0; null while either is unknown. */
fun blocksRemaining(record: CloseRecordView): Long? {
    val claimable = record.claimableAtHeight?.toLong() ?: return null
    val tip = record.currentHeight?.toLong() ?: return null
    return maxOf(0L, claimable - tip)
}

/**
 * `humanizeBlocks` (`close-records/estimate.ts:60-66`): 10 minutes a block;
 * minutes under an hour, rounded hours under 48, rounded days after.
 */
fun humanizeBlocks(blocks: Long): String {
    val minutes = blocks * 10
    if (minutes < 60) return "~$minutes minutes"
    val hours = (minutes / 60.0).roundToLong()
    if (hours < 48) return "~$hours ${if (hours == 1L) "hour" else "hours"}"
    return "~${(hours / 24.0).roundToLong()} days"
}

/** Total fees row: sum of the known per-tx fees (`ChannelCloseDetail.tsx:147`). */
fun totalFeesSats(record: CloseRecordView): Long =
    record.txs.sumOf { (it.feeSats ?: 0uL).toLong() }

/**
 * Needs-deposit is derived from RecoveryState at render — never stored on
 * the record (`ChannelCloseDetail.tsx:141-142`).
 */
fun needsDeposit(recovery: RecoveryStateView?, channelId: String): Boolean =
    recovery?.status == RecoveryStatusView.NEEDS_RECOVERY &&
        recovery.channelIds.contains(channelId)

/**
 * Per-tx confirmation caption (`ChannelCloseDetail.tsx:73-77`): unconfirmed,
 * a live count when the core computed one, or a bare "Confirmed" when the
 * tip is unknown.
 */
fun confirmationText(tx: CloseTxView): String {
    if (tx.confirmedAtHeight == null) return "Unconfirmed"
    val confs = tx.confirmations?.toLong() ?: return "Confirmed"
    return "$confs conf${if (confs == 1L) "" else "s"}"
}

/**
 * `Intl.DateTimeFormat('en-GB', {day:'numeric', month:'long',
 * year:'numeric', hour:'2-digit', minute:'2-digit'})`
 * (`ChannelCloseDetail.tsx:32-40`).
 */
fun formatCloseDate(timestampMs: Long, zone: ZoneId = ZoneId.systemDefault()): String =
    DateTimeFormatter.ofPattern("d MMMM yyyy 'at' HH:mm", Locale.UK)
        .format(Instant.ofEpochMilli(timestampMs).atZone(zone))

// ---------------------------------------------------------------------------
// Home (Home.tsx, RecoveryBanner.tsx, PendingSweepBanner.tsx)
// ---------------------------------------------------------------------------

/** What Home's BalanceDisplay renders, per `use-unified-balance.ts`. */
data class HomeBalance(
    /** Full on-chain (confirmed + all pending) + floored Lightning sats. */
    val totalSats: Long,
    /** The `+₿X pending` line: unconfirmed external receives only. */
    val pendingSats: Long,
)

fun homeBalance(balances: Balances): HomeBalance = HomeBalance(
    totalSats = msatToSatFloor(balances.lightningMsat.toLong()) +
        balances.onchainTotalSats.toLong(),
    pendingSats = balances.onchainUntrustedPendingSats.toLong(),
)

/** One of `RecoveryBanner.tsx`'s two variants, fully derived. */
data class RecoveryBannerUi(
    val title: String,
    val subtitle: String,
    /** Needs-recovery taps through to the Recover screen. */
    val navigatesToRecover: Boolean,
    /** Only the sweep-confirmed success banner can be dismissed. */
    val dismissible: Boolean,
)

/**
 * RecoveryBanner gating (`Home.tsx:80-84` + `RecoveryBanner.tsx`): shown
 * whenever recovery state exists; a dismissal only ever hides the
 * sweep-confirmed success variant (the needs-recovery banner has no dismiss
 * affordance).
 */
fun recoveryBanner(recovery: RecoveryStateView?, dismissed: Boolean): RecoveryBannerUi? {
    if (recovery == null) return null
    return when (recovery.status) {
        RecoveryStatusView.SWEEP_CONFIRMED -> {
            if (dismissed) return null
            RecoveryBannerUi(
                title = "Funds recovered!",
                subtitle = "Available in approximately 14 days",
                navigatesToRecover = false,
                dismissible = true,
            )
        }
        RecoveryStatusView.NEEDS_RECOVERY -> RecoveryBannerUi(
            title = "Your funds are safe",
            subtitle = "A small deposit is needed to unlock them",
            navigatesToRecover = true,
            dismissible = false,
        )
    }
}

/** The `PendingSweepBanner.tsx` heading/subtext/deep-link matrix, derived. */
data class SweepBannerUi(
    val heading: String,
    val subtitle: String,
    /** Needs-on-chain-funds taps through to Receive to add the deposit. */
    val navigatesToReceive: Boolean,
)

/**
 * PendingSweepBanner (`Home.tsx:86-90` + `PendingSweepBanner.tsx`): only
 * after a failed sweep attempt; `pendingSats` is a lower bound, so unknown
 * values get an "At least" prefix, and a shortfall is a floor phrased as
 * "at least".
 */
fun sweepBanner(info: PendingSweepView?): SweepBannerUi? {
    if (info == null || !info.lastAttemptFailed) return null
    val amount = if (info.pendingSats > 0uL) formatBtc(info.pendingSats.toLong()) else null
    val heading = when {
        amount == null -> "Funds waiting to sweep"
        info.hasUnknownValue -> "At least $amount waiting to sweep"
        else -> "$amount waiting to sweep"
    }
    if (info.needsOnchainFunds) {
        val shortfall = info.shortfallSats
        val subtitle = if (shortfall != null && shortfall > 0uL) {
            "Add at least ${formatBtc(shortfall.toLong())} " +
                "to cover network fees and recover these funds"
        } else {
            "Add bitcoin to cover network fees and recover these funds"
        }
        return SweepBannerUi(heading, subtitle, navigatesToReceive = true)
    }
    return SweepBannerUi(
        heading = heading,
        subtitle = "Recovered funds return to your balance automatically when network fees allow",
        navigatesToReceive = false,
    )
}
