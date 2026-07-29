package zinqq.app.screens.settings

import uniffi.wallet_core.ChannelStateLabel
import uniffi.wallet_core.ChannelView
import uniffi.wallet_core.PeerView
import uniffi.wallet_core.WalletException
import zinqq.main.formatBtc
import zinqq.main.msatToSatFloor

/**
 * The Peers screen's pure derivations (U17, R10 UI, R14): the PWA's
 * client-side `parsePeerAddress` (`peer-connection.ts:149-174`, verbatim
 * copy), the header count label, per-peer presentation, and the nested
 * channel rows (`Peers.tsx`). The connect itself happens inside the core's
 * `open_channel` (connect-if-needed), exactly like the PWA's OpenChannel.
 */

private val PUBKEY_HEX_RE = Regex("^[0-9a-f]{66}$")
private val HOST_RE = Regex("^[a-zA-Z0-9._-]+$")

sealed interface PeerAddressParse {
    data class Valid(val pubkey: String, val host: String, val port: Int) : PeerAddressParse
    data class Invalid(val message: String) : PeerAddressParse
}

/** `parsePeerAddress` (`peer-connection.ts:149-174`), error strings verbatim. */
fun parsePeerAddress(address: String): PeerAddressParse {
    val atIndex = address.indexOf('@')
    if (atIndex == -1) {
        return PeerAddressParse.Invalid("Invalid peer address: expected pubkey@host:port")
    }
    val pubkey = address.substring(0, atIndex)
    val hostPort = address.substring(atIndex + 1)
    val colonIndex = hostPort.lastIndexOf(':')
    if (colonIndex == -1) {
        return PeerAddressParse.Invalid("Invalid peer address: expected host:port after @")
    }
    val host = hostPort.substring(0, colonIndex)
    val port = hostPort.substring(colonIndex + 1).toIntOrNull()
    if (port == null || port < 1 || port > 65535) {
        return PeerAddressParse.Invalid(
            "Invalid peer address: port must be a number between 1 and 65535",
        )
    }
    if (!PUBKEY_HEX_RE.matches(pubkey)) {
        return PeerAddressParse.Invalid(
            "Invalid peer address: pubkey must be 66 lowercase hex characters",
        )
    }
    if (!HOST_RE.matches(host)) {
        return PeerAddressParse.Invalid(
            "Invalid peer address: host must contain only alphanumeric, dot, hyphen, or underscore",
        )
    }
    return PeerAddressParse.Valid(pubkey, host, port)
}

/** `Peers ({connectedCount} connected, {peers.length} saved)` (`Peers.tsx:199`). */
fun peersCountLabel(peers: List<PeerView>): String =
    "Peers (${peers.count { it.connected }} connected, ${peers.size} saved)"

/** `{pubkey.slice(0, 16)}...{pubkey.slice(-8)}` (`Peers.tsx:226`). */
fun peerDisplayId(pubkey: String): String = pubkey.take(16) + "..." + pubkey.takeLast(8)

/** `Connected` / `Offline` (`Peers.tsx:233`). */
fun peerStatusLabel(connected: Boolean): String = if (connected) "Connected" else "Offline"

/** Forget renders only for saved (known) peers (`Peers.tsx:235`). */
fun showsForget(peer: PeerView): Boolean = peer.known

/** Forget is disabled while channels with the peer are open (`Peers.tsx:239`). */
fun forgetEnabled(peer: PeerView): Boolean = peer.channelCount == 0u

/**
 * Forget-failure copy: the typed guard carries the PWA's exact string
 * (`context.tsx:866` via the core); anything else surfaces as-is
 * (`Peers.tsx:129-131`).
 */
fun forgetErrorMessage(e: Throwable): String = when (e) {
    is WalletException.PeerHasOpenChannels -> "Cannot forget peer with open channels"
    is WalletException.NotRunning -> "the node is not running"
    else -> e.message?.takeIf { it.isNotBlank() } ?: "Failed to forget peer"
}

/** The nested rows: channels grouped under their peer (`Peers.tsx:52-77`). */
fun channelsByPeer(channels: List<ChannelView>): Map<String, List<ChannelView>> =
    channels.groupBy { it.counterpartyPubkey }

/** State label (`Peers.tsx:263-269`), including the `Closing…` ellipsis. */
fun channelStateText(state: ChannelStateLabel): String = when (state) {
    ChannelStateLabel.ACTIVE -> "Active"
    ChannelStateLabel.READY -> "Ready"
    ChannelStateLabel.PENDING -> "Pending"
    ChannelStateLabel.CLOSING -> "Closing…"
}

/**
 * The stalled-coop-close note under a `Closing…` row (`Peers.tsx:275-279`).
 */
const val CLOSING_IN_PROGRESS_NOTE =
    "Cooperative close in progress. If it doesn't complete (LSP offline), " +
        "you can force close instead."

/** `Force Close` while shutting down, `Close` otherwise (`Peers.tsx:301`). */
fun channelCloseActionLabel(state: ChannelStateLabel): String =
    if (state == ChannelStateLabel.CLOSING) "Force Close" else "Close"

/** `{formatBtc(capacity)} capacity` (`Peers.tsx:271-273`). */
fun channelCapacityText(channel: ChannelView): String =
    "${formatBtc(channel.capacitySats.toLong())} capacity"

/** `Send: ₿X` (`Peers.tsx:283`) — msat floored, never overstated. */
fun channelSendText(channel: ChannelView): String =
    "Send: ${formatBtc(msatToSatFloor(channel.outboundMsat.toLong()))}"

/** `Receive: ₿X` (`Peers.tsx:284`). */
fun channelReceiveText(channel: ChannelView): String =
    "Receive: ${formatBtc(msatToSatFloor(channel.inboundMsat.toLong()))}"

/** `Reserve: ₿X`, only when the core knows the reserve (`Peers.tsx:285-287`). */
fun channelReserveText(channel: ChannelView): String? =
    channel.reserveSats?.let { "Reserve: ${formatBtc(it.toLong())}" }
