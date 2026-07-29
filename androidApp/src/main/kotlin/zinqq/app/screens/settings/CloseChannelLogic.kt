package zinqq.app.screens.settings

import uniffi.wallet_core.CloseEstimate
import uniffi.wallet_core.CloseFeePayer
import uniffi.wallet_core.WalletException
import zinqq.app.humanizeBlocks
import zinqq.main.formatBtc

/**
 * CloseChannel's pure derivations (U17, R10 UI, R14): the coop/force
 * confirm variants over the core's informational [CloseEstimate] — every
 * field independently nullable, rendering placeholders and NEVER blocking
 * the close (`CloseChannel.tsx:49-54,276-293`) — plus the warnings, the
 * success copy, and the force-close escalation offer.
 */

/** `Estimated Cost to You` (`CloseChannel.tsx:276-281`). */
fun closeCostLabel(estimate: CloseEstimate?, force: Boolean, loading: Boolean): String {
    if (loading) return "Estimating…"
    val cost = if (force) estimate?.forceTotalYouPaySats else estimate?.coopTotalYouPaySats
    return cost?.let { "~${formatBtc(it.toLong())}" } ?: "Estimate unavailable"
}

/** `Funds Available` (`CloseChannel.tsx:282-286`). */
fun closeTimelineLabel(estimate: CloseEstimate?, force: Boolean): String {
    if (!force) return "~minutes once confirmed"
    val blocks = estimate?.timelockBlocks ?: return "up to ~14 days"
    return "up to ${humanizeBlocks(blocks.toLong())}"
}

/** `You Get Back` (`CloseChannel.tsx:287-291`). */
fun expectedBackLabel(estimate: CloseEstimate?, loading: Boolean): String {
    if (loading) return "Estimating…"
    return estimate?.expectedBackSats?.let { "~${formatBtc(it.toLong())}" } ?: "—"
}

/** The LSP-pays note gate (`CloseChannel.tsx:292,339-343`). */
fun lspPaysCloseFee(estimate: CloseEstimate?, force: Boolean): Boolean =
    !force && estimate?.feePayer == CloseFeePayer.COUNTERPARTY

/** `The closing fee is paid by the LSP…` (`CloseChannel.tsx:340-342`). */
const val LSP_PAYS_NOTE = "The closing fee is paid by the LSP — this close costs you nothing."

/** The always-shown estimate caveat (`CloseChannel.tsx:344-346`). */
const val ESTIMATE_CAVEAT =
    "Estimate at current network fees; the final cost varies with network conditions."

/** Non-anchor warning gate: force close of a known non-anchor channel only. */
fun showsNonAnchorWarning(estimate: CloseEstimate?, force: Boolean): Boolean =
    force && estimate?.isAnchor == false

/** `CloseChannel.tsx:399-404`. */
const val NON_ANCHOR_WARNING =
    "This channel doesn't support anchor outputs, so the force-close transaction " +
        "can't be fee-bumped. If network fees spike, confirmation may take much longer."

/** The `N in-flight payment(s)` warning (`CloseChannel.tsx:406-413`); null = hidden. */
fun pendingHtlcWarning(estimate: CloseEstimate?): String? {
    val count = estimate?.pendingHtlcCount?.toInt() ?: 0
    if (count == 0) return null
    val payments = if (count == 1) "1 in-flight payment" else "$count in-flight payments"
    return "$payments must settle before the close completes — " +
        "the amount returned may change."
}

/** The force info box (`CloseChannel.tsx:386-391`); embeds the live timeline. */
fun forceCloseInfoText(estimate: CloseEstimate?): String =
    "Force closing moves your balance on-chain without the LSP's cooperation. " +
        "It may cost more, and your funds are locked for " +
        closeTimelineLabel(estimate, force = true) +
        " while the network verifies the close. You wait; the other side doesn't."

/** The cooperative info box (`CloseChannel.tsx:392-397`). */
const val COOP_CLOSE_INFO =
    "Closing this channel moves your balance back to your on-chain wallet and " +
        "incurs an on-chain fee. The LSP must be online — keep the app open until " +
        "the close completes."

/** The CTA (`CloseChannel.tsx:424`). */
fun closeCtaLabel(force: Boolean, closing: Boolean): String = when {
    closing -> "Closing…"
    force -> "Force Close Channel"
    else -> "Close Channel"
}

/** The success screen's detail copy (`CloseChannel.tsx:192-206`). */
fun closeSuccessDetail(force: Boolean, estimate: CloseEstimate?): String {
    if (!force) {
        return "Your channel is closing. Funds return to your wallet once the closing " +
            "transaction confirms on-chain — keep the app open until the close completes."
    }
    val timeline = estimate?.timelockBlocks?.let { humanizeBlocks(it.toLong()) } ?: "~14 days"
    return "Force close initiated. Your funds will be accessible in $timeline — they " +
        "return to your wallet automatically once the timelock expires."
}

/** A failed close: the message plus the coop → force escalation offer. */
data class CloseFailureUi(val message: String, val canForceClose: Boolean)

/**
 * Failure mapping (`CloseChannel.tsx:135-156`): typed core details surface;
 * unknown failures fall back to the PWA's `ok === false` copy per variant.
 * Only a failed COOPERATIVE close offers "Force Close Instead".
 */
fun closeFailure(e: Throwable, force: Boolean): CloseFailureUi {
    val message = when (e) {
        is WalletException.ChannelCloseFailed -> "Close failed: ${e.detail}"
        is WalletException.ChannelNotFound -> "Channel not found"
        is WalletException.NotRunning -> "the node is not running"
        else -> e.message?.takeIf { it.isNotBlank() } ?: if (force) {
            "Force close failed."
        } else {
            "Cooperative close failed. The peer may be disconnected or the channel " +
                "has pending payments."
        }
    }
    return CloseFailureUi(message = message, canForceClose = !force)
}
