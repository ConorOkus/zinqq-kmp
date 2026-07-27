import Foundation
import Shared

/// CloseChannel's pure derivations (U22, R10 UI, R14): the coop/force
/// confirm variants over the core's informational `CloseEstimate` — every
/// field independently nullable, rendering placeholders and NEVER blocking
/// the close (`CloseChannel.tsx:49-54,276-293`) — plus the warnings, the
/// success copy, and the force-close escalation offer. Ported check-for-check
/// from Android's `CloseChannelLogic.kt`.

/// `Estimated Cost to You` (`CloseChannel.tsx:276-281`).
func closeCostLabel(_ estimate: CloseEstimate?, force: Bool, loading: Bool) -> String {
    if loading { return "Estimating…" }
    let cost = force ? estimate?.forceTotalYouPaySats : estimate?.coopTotalYouPaySats
    guard let cost = cost?.int64Value else { return "Estimate unavailable" }
    return "~\(FormatKt.formatBtc(sats: cost))"
}

/// `Funds Available` (`CloseChannel.tsx:282-286`).
func closeTimelineLabel(_ estimate: CloseEstimate?, force: Bool) -> String {
    if !force { return "~minutes once confirmed" }
    guard let blocks = estimate?.timelockBlocks?.int64Value else { return "up to ~14 days" }
    return "up to \(humanizeBlocks(blocks))"
}

/// `You Get Back` (`CloseChannel.tsx:287-291`).
func expectedBackLabel(_ estimate: CloseEstimate?, loading: Bool) -> String {
    if loading { return "Estimating…" }
    guard let back = estimate?.expectedBackSats?.int64Value else { return "—" }
    return "~\(FormatKt.formatBtc(sats: back))"
}

/// The LSP-pays note gate (`CloseChannel.tsx:292,339-343`).
func lspPaysCloseFee(_ estimate: CloseEstimate?, force: Bool) -> Bool {
    !force && estimate?.feePayer == CloseFeePayer.counterparty
}

/// `The closing fee is paid by the LSP…` (`CloseChannel.tsx:340-342`).
let lspPaysNote = "The closing fee is paid by the LSP — this close costs you nothing."

/// The always-shown estimate caveat (`CloseChannel.tsx:344-346`).
let estimateCaveat =
    "Estimate at current network fees; the final cost varies with network conditions."

/// Non-anchor warning gate: force close of a known non-anchor channel only.
func showsNonAnchorWarning(_ estimate: CloseEstimate?, force: Bool) -> Bool {
    force && estimate?.isAnchor?.boolValue == false
}

/// `CloseChannel.tsx:399-404`.
let nonAnchorWarning =
    "This channel doesn't support anchor outputs, so the force-close transaction "
        + "can't be fee-bumped. If network fees spike, confirmation may take much longer."

/// The `N in-flight payment(s)` warning (`CloseChannel.tsx:406-413`); nil = hidden.
func pendingHtlcWarning(_ estimate: CloseEstimate?) -> String? {
    let count = estimate?.pendingHtlcCount?.int32Value ?? 0
    if count == 0 { return nil }
    let payments = count == 1 ? "1 in-flight payment" : "\(count) in-flight payments"
    return "\(payments) must settle before the close completes — "
        + "the amount returned may change."
}

/// The force info box (`CloseChannel.tsx:386-391`); embeds the live timeline.
func forceCloseInfoText(_ estimate: CloseEstimate?) -> String {
    "Force closing moves your balance on-chain without the LSP's cooperation. "
        + "It may cost more, and your funds are locked for "
        + closeTimelineLabel(estimate, force: true)
        + " while the network verifies the close. You wait; the other side doesn't."
}

/// The cooperative info box (`CloseChannel.tsx:392-397`).
let coopCloseInfo =
    "Closing this channel moves your balance back to your on-chain wallet and "
        + "incurs an on-chain fee. The LSP must be online — keep the app open until "
        + "the close completes."

/// The CTA (`CloseChannel.tsx:424`).
func closeCtaLabel(force: Bool, closing: Bool) -> String {
    if closing { return "Closing…" }
    return force ? "Force Close Channel" : "Close Channel"
}

/// The success screen's detail copy (`CloseChannel.tsx:192-206`).
func closeSuccessDetail(force: Bool, estimate: CloseEstimate?) -> String {
    if !force {
        return "Your channel is closing. Funds return to your wallet once the closing "
            + "transaction confirms on-chain — keep the app open until the close completes."
    }
    let timeline = estimate?.timelockBlocks.map { humanizeBlocks($0.int64Value) } ?? "~14 days"
    return "Force close initiated. Your funds will be accessible in \(timeline) — they "
        + "return to your wallet automatically once the timelock expires."
}

/// A failed close: the message plus the coop → force escalation offer.
struct CloseFailureUi: Equatable {
    let message: String
    let canForceClose: Bool
}

/// Failure mapping (`CloseChannel.tsx:135-156`): typed core details surface;
/// unknown failures fall back to the PWA's `ok === false` copy per variant.
/// Only a failed COOPERATIVE close offers "Force Close Instead".
func closeFailure(_ e: KotlinThrowable, force: Bool) -> CloseFailureUi {
    let message: String
    switch e {
    case let e as WalletException.ChannelCloseFailed:
        message = "Close failed: \(e.detail)"
    case is WalletException.ChannelNotFound:
        message = "Channel not found"
    case is WalletException.NotRunning:
        message = "the node is not running"
    default:
        if let m = e.message, !m.isEmpty {
            message = m
        } else if force {
            message = "Force close failed."
        } else {
            message = "Cooperative close failed. The peer may be disconnected or the channel "
                + "has pending payments."
        }
    }
    return CloseFailureUi(message: message, canForceClose: !force)
}

/// Bridged variant for the Swift `Error` the async FFI throws.
func closeFailure(_ error: Error, force: Bool) -> CloseFailureUi {
    if let kotlin = kotlinThrowable(error) { return closeFailure(kotlin, force: force) }
    let description = (error as NSError).localizedDescription
    if !description.isEmpty {
        return CloseFailureUi(message: description, canForceClose: !force)
    }
    return closeFailure(KotlinRuntimeException(message: nil), force: force)
}
