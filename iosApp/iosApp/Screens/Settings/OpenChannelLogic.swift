import Foundation
import Shared

/// OpenChannel's pure derivations (U22, R10 UI, R14): the PWA's bounds and
/// balance gate (`OpenChannel.tsx:29-34,83-111`), the review labels, and the
/// typed-error copy. The core re-enforces the same bounds/balance gate inside
/// `openChannel` — these are the PWA's pre-review inline errors. Ported
/// check-for-check from Android's `OpenChannelLogic.kt`.

/// `MIN_CHANNEL_SATS` (`OpenChannel.tsx:29`).
let minChannelSats: UInt64 = 20_000

/// `MAX_CHANNEL_SATS` — LDK non-wumbo limit (`OpenChannel.tsx:31`).
let maxChannelSats: UInt64 = 16_777_215

/// `MAX_DIGITS` (`OpenChannel.tsx:32`).
let openAmountMaxDigits = 8

/// The amount step's gate (`OpenChannel.tsx:83-111`), PWA copy verbatim;
/// `nil` = proceed to review.
func validateOpenAmount(
    amountSats: UInt64,
    estimatedFeeSats: UInt64,
    balanceSats: UInt64
) -> String? {
    if amountSats < minChannelSats {
        return "Minimum channel size is \(FormatKt.formatBtc(sats: Int64(bitPattern: minChannelSats)))"
    }
    if amountSats > maxChannelSats {
        return "Maximum channel size is \(FormatKt.formatBtc(sats: Int64(bitPattern: maxChannelSats)))"
    }
    if amountSats + estimatedFeeSats > balanceSats {
        return "Amount plus fees exceeds available balance"
    }
    return nil
}

/// `Est. fee (~{rate} sat/vB)` (`OpenChannel.tsx:263-265`).
func openFeeRateLabel(_ feeRateSatPerVb: UInt64) -> String {
    "Est. fee (~\(feeRateSatPerVb) sat/vB)"
}

/// The review Total row (`OpenChannel.tsx:272`).
func openTotalSats(amountSats: UInt64, estimatedFeeSats: UInt64) -> UInt64 {
    amountSats + estimatedFeeSats
}

/// The PWA's fee-fetch fallback (`OpenChannel.tsx:70-72,97-98`): 1 sat/vB ×
/// the 140 vB approximate funding-tx vsize, used when `estimateOpenFee`
/// fails (e.g. stopped node).
func fallbackOpenFee() -> OpenFeeEstimate {
    OpenFeeEstimate(feeRateSatPerVb: 1, estimatedFeeSats: 140)
}

/// `{pubkey.slice(0, 12)}...{pubkey.slice(-8)}` (`OpenChannel.tsx:253`).
func reviewPeerDisplay(_ pubkey: String) -> String {
    String(pubkey.prefix(12)) + "..." + String(pubkey.suffix(8))
}

/// Open-failure copy: the typed channel errors carry the core's PWA-parity
/// strings; unknown failures fall back to the PWA's `ok === false` copy
/// (`OpenChannel.tsx:138-141`).
func openChannelErrorMessage(_ e: KotlinThrowable) -> String {
    switch e {
    case is WalletException.ChannelAmountBelowMinimum:
        return "Minimum channel size is \(FormatKt.formatBtc(sats: Int64(bitPattern: minChannelSats)))"
    case is WalletException.ChannelAmountAboveMaximum:
        return "Maximum channel size is \(FormatKt.formatBtc(sats: Int64(bitPattern: maxChannelSats)))"
    case is WalletException.ChannelAmountExceedsBalance:
        return "Amount plus fees exceeds available balance"
    case let e as WalletException.InvalidPeerAddress:
        return e.detail
    case let e as WalletException.PeerConnectFailed:
        return "Failed to connect to peer: \(e.detail)"
    case let e as WalletException.PeerPersistFailed:
        return "Failed to persist known peer: \(e.detail)"
    case let e as WalletException.ChannelOpenFailed:
        return "Failed to initiate channel opening: \(e.detail)"
    case is WalletException.NotRunning:
        return "the node is not running"
    default:
        if let message = e.message, !message.isEmpty { return message }
        return "Failed to initiate channel opening. The peer may have disconnected."
    }
}

/// Bridged variant for the Swift `Error` the async FFI throws.
func openChannelErrorMessage(_ error: Error) -> String {
    if let kotlin = kotlinThrowable(error) { return openChannelErrorMessage(kotlin) }
    let description = (error as NSError).localizedDescription
    if !description.isEmpty { return description }
    return "Failed to initiate channel opening. The peer may have disconnected."
}
