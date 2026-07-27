import Foundation
import Shared

/// The Peers screen's pure derivations (U22, R10 UI, R14): the PWA's
/// client-side `parsePeerAddress` (`peer-connection.ts:149-174`, verbatim
/// copy), the header count label, per-peer presentation, and the nested
/// channel rows (`Peers.tsx`). The connect itself happens inside the core's
/// `openChannel` (connect-if-needed), exactly like the PWA's OpenChannel.
/// Ported check-for-check from Android's `PeersLogic.kt`.

private let pubkeyHexPattern = "^[0-9a-f]{66}$"
private let hostPattern = "^[a-zA-Z0-9._-]+$"

enum PeerAddressParse: Equatable {
    case valid(pubkey: String, host: String, port: Int)
    case invalid(message: String)
}

/// `parsePeerAddress` (`peer-connection.ts:149-174`), error strings verbatim.
func parsePeerAddress(_ address: String) -> PeerAddressParse {
    guard let atIndex = address.firstIndex(of: "@") else {
        return .invalid(message: "Invalid peer address: expected pubkey@host:port")
    }
    let pubkey = String(address[..<atIndex])
    let hostPort = String(address[address.index(after: atIndex)...])
    guard let colonIndex = hostPort.lastIndex(of: ":") else {
        return .invalid(message: "Invalid peer address: expected host:port after @")
    }
    let host = String(hostPort[..<colonIndex])
    let portString = String(hostPort[hostPort.index(after: colonIndex)...])
    guard let port = Int(portString), (1...65535).contains(port) else {
        return .invalid(
            message: "Invalid peer address: port must be a number between 1 and 65535"
        )
    }
    guard pubkey.range(of: pubkeyHexPattern, options: .regularExpression) != nil else {
        return .invalid(
            message: "Invalid peer address: pubkey must be 66 lowercase hex characters"
        )
    }
    guard host.range(of: hostPattern, options: .regularExpression) != nil else {
        return .invalid(
            message: "Invalid peer address: host must contain only alphanumeric, dot, "
                + "hyphen, or underscore"
        )
    }
    return .valid(pubkey: pubkey, host: host, port: port)
}

/// `Peers ({connectedCount} connected, {peers.length} saved)` (`Peers.tsx:199`).
func peersCountLabel(_ peers: [PeerView]) -> String {
    "Peers (\(peers.filter(\.connected).count) connected, \(peers.count) saved)"
}

/// `{pubkey.slice(0, 16)}...{pubkey.slice(-8)}` (`Peers.tsx:226`).
func peerDisplayId(_ pubkey: String) -> String {
    String(pubkey.prefix(16)) + "..." + String(pubkey.suffix(8))
}

/// `Connected` / `Offline` (`Peers.tsx:233`).
func peerStatusLabel(connected: Bool) -> String { connected ? "Connected" : "Offline" }

/// Forget renders only for saved (known) peers (`Peers.tsx:235`).
func showsForget(_ peer: PeerView) -> Bool { peer.known }

/// Forget is disabled while channels with the peer are open (`Peers.tsx:239`).
func forgetEnabled(_ peer: PeerView) -> Bool { peer.channelCount == 0 }

/// Forget-failure copy: the typed guard carries the PWA's exact string
/// (`context.tsx:866` via the core); anything else surfaces as-is
/// (`Peers.tsx:129-131`).
func forgetErrorMessage(_ e: KotlinThrowable) -> String {
    if e is WalletException.PeerHasOpenChannels { return "Cannot forget peer with open channels" }
    if e is WalletException.NotRunning { return "the node is not running" }
    if let message = e.message, !message.isEmpty { return message }
    return "Failed to forget peer"
}

/// Bridged variant for the Swift `Error` the async FFI throws.
func forgetErrorMessage(_ error: Error) -> String {
    if let kotlin = kotlinThrowable(error) { return forgetErrorMessage(kotlin) }
    let description = (error as NSError).localizedDescription
    return description.isEmpty ? "Failed to forget peer" : description
}

/// The nested rows: channels grouped under their peer (`Peers.tsx:52-77`).
func channelsByPeer(_ channels: [ChannelView]) -> [String: [ChannelView]] {
    Dictionary(grouping: channels, by: { $0.counterpartyPubkey })
}

/// State label (`Peers.tsx:263-269`), including the `Closing…` ellipsis.
func channelStateText(_ state: ChannelStateLabel) -> String {
    if state == .active { return "Active" }
    if state == .ready { return "Ready" }
    if state == .pending { return "Pending" }
    return "Closing…"
}

/// The stalled-coop-close note under a `Closing…` row (`Peers.tsx:275-279`).
let closingInProgressNote =
    "Cooperative close in progress. If it doesn't complete (LSP offline), "
        + "you can force close instead."

/// `Force Close` while shutting down, `Close` otherwise (`Peers.tsx:301`).
func channelCloseActionLabel(_ state: ChannelStateLabel) -> String {
    state == .closing ? "Force Close" : "Close"
}

/// `{formatBtc(capacity)} capacity` (`Peers.tsx:271-273`).
func channelCapacityText(_ channel: ChannelView) -> String {
    "\(FormatKt.formatBtc(sats: Int64(bitPattern: channel.capacitySats))) capacity"
}

/// `Send: ₿X` (`Peers.tsx:283`) — msat floored, never overstated.
func channelSendText(_ channel: ChannelView) -> String {
    let sats = FormatKt.msatToSatFloor(msat: Int64(bitPattern: channel.outboundMsat))
    return "Send: \(FormatKt.formatBtc(sats: sats))"
}

/// `Receive: ₿X` (`Peers.tsx:284`).
func channelReceiveText(_ channel: ChannelView) -> String {
    let sats = FormatKt.msatToSatFloor(msat: Int64(bitPattern: channel.inboundMsat))
    return "Receive: \(FormatKt.formatBtc(sats: sats))"
}

/// `Reserve: ₿X`, only when the core knows the reserve (`Peers.tsx:285-287`).
func channelReserveText(_ channel: ChannelView) -> String? {
    guard let reserve = channel.reserveSats?.int64Value else { return nil }
    return "Reserve: \(FormatKt.formatBtc(sats: reserve))"
}
