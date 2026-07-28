import Foundation
import Shared

/// The pure half of the receive flow (U21, F2, R6 UI, R12): the PWA's
/// `Receive.tsx` state machine as immutable step data plus pure gating /
/// derivation functions, ported function-for-function from Android's
/// `ReceiveFlow.kt`. NO liquidity logic lives here — the capacity decision
/// (`needs_jit`), the live floor, quote fees, and the invoice-expiry clamp all
/// arrive as core results (`ReceiveBundle`, `JitQuote`, `JitInvoice`, typed
/// `WalletException`s); these functions only route between steps and carry the
/// PWA's copy. `ReceiveController` owns the FFI calls.

/// PWA `Receive.tsx:30` (`MAX_DIGITS`): receive amounts cap at 8 digits.
let receiveMaxDigits = 8

/// PWA `Receive.tsx:401`: "Copied!" feedback duration.
let copyFeedbackMs: UInt64 = 2_000

/// Which invoice the unified QR currently embeds (PWA `InvoicePath`).
enum InvoicePath {
    case none
    case standard
    case jit
}

/// The snap pager's two pages (PWA `QrPage`).
enum QrPage: Int, CaseIterable {
    case unified = 0
    case bolt12 = 1
}

/// `jit-review` (kind `commit`): fee disclosure before committing LSP-side
/// liquidity. Fee rows ceil like the PWA (`Receive.tsx:732`).
struct ReceiveJitReview: Equatable {
    let amountSats: UInt64
    let quote: JitQuote
    /// The PWA's `quoteStatus: 'updated'` — this quote replaced a stale one.
    var quoteUpdated: Bool = false

    var setupFeeSats: UInt64 {
        UInt64(FormatKt.msatToSatCeil(msat: Int64(bitPattern: quote.openingFeeMsat)))
    }

    var youReceiveSats: UInt64 { amountSats - setupFeeSats }
}

/// One step of the PWA's receive machine (`Receive.tsx:43-66`), case-for-case
/// with Android's `ReceiveStep` sealed interface.
enum ReceiveStep: Equatable {
    /// `ready`: the QR screen; `invoicePath` says what the URI embeds.
    case display(invoicePath: InvoicePath)
    /// `jit-quoting`: Phase A in flight — review skeleton renders.
    case quoting
    /// `jit-review` (kind `commit`).
    case jitReview(ReceiveJitReview)
    /// `jit-review` (kind `below-minimum`): disabled CTA + suggested floor.
    case jitBelowMinimum(amountSats: UInt64, displayMinSats: UInt64)
    /// `jit-buying`: Phase B in flight.
    case buying
    /// `jit-error`: quote/buy failed; retry re-runs Phase A.
    case jitError
    /// `jit-expired`: the displayed JIT invoice outlived its quote validity.
    case jitExpired
    /// `success`: an inbound payment matching our invoice settled.
    case received(amountSats: UInt64)
}

// MARK: - Capacity + floor gating (AE4)

/// PWA `Receive.tsx:33-39`: sum of inbound capacity across usable channels.
/// (Named `sum…` because Swift member lookup would otherwise collide with
/// `ReceivePort.usableInboundMsat()` inside its conformers.)
func sumUsableInboundMsat(_ channels: [ChannelView]) -> UInt64 {
    channels.filter(\.usable).reduce(0) { $0 + $1.inboundMsat }
}

/// Whether the amount being typed would require a JIT channel
/// (PWA `Receive.tsx:121-122` `editingNeedsJit`).
func editingNeedsJit(usableInboundMsat: UInt64, amountSats: UInt64) -> Bool {
    usableInboundMsat < amountSats * 1_000
}

/// AE4 / PWA `Receive.tsx:133-134`: block advancing past the numpad for a JIT
/// receive below the floor the LSP can service. On-chain / in-capacity
/// receives are unaffected.
func belowJitMinimum(needsJit: Bool, amountSats: UInt64, floorSats: UInt64) -> Bool {
    needsJit && amountSats > 0 && amountSats < floorSats
}

/// PWA `Receive.tsx:927` (`nextDisabled`), inverted.
func numpadNextEnabled(amountSats: UInt64, belowMinimum: Bool) -> Bool {
    amountSats > 0 && !belowMinimum
}

/// PWA `Receive.tsx:928`: 'Request' on the mandatory first entry, else 'Done'.
func numpadCtaLabel(needsAmount: Bool, confirmedAmountSats: UInt64) -> String {
    needsAmount && confirmedAmountSats <= 0 ? "Request" : "Done"
}

/// PWA `Receive.tsx:918-921`: the below-floor numpad alert (AE4 copy).
func minimumAlertText(floorSats: UInt64) -> String {
    "Minimum \(FormatKt.formatBtc(sats: Int64(bitPattern: floorSats)))"
}

/// Numpad Next routing (PWA `handleConfirmAmount`, `Receive.tsx:425-439`):
/// blocked below the JIT floor (defense in depth — the button is also
/// disabled); otherwise commit, flipping straight to the quoting skeleton
/// when the amount needs JIT so no stale QR frame renders.
enum ConfirmDecision: Equatable {
    case blocked
    case request(amountSats: UInt64, presentQuoting: Bool)
}

/// (Named `…Decision` because Swift member lookup would otherwise collide
/// with `ReceiveController.confirmAmount()` inside the controller.)
func confirmAmountDecision(
    amountSats: UInt64,
    usableInboundMsat: UInt64,
    floorSats: UInt64
) -> ConfirmDecision {
    let needsJit = editingNeedsJit(usableInboundMsat: usableInboundMsat, amountSats: amountSats)
    if belowJitMinimum(needsJit: needsJit, amountSats: amountSats, floorSats: floorSats) {
        return .blocked
    }
    return .request(amountSats: amountSats, presentQuoting: needsJit && amountSats > 0)
}

// MARK: - Pager + captions

/// Pager eligibility (PWA `Receive.tsx:372` `showBolt12`): the offer page
/// exists only when the core produced an offer (which it does only with ≥1
/// usable channel) AND the visit is not the no-channel mandatory-amount case.
func showBolt12Page(offerExists: Bool, needsAmount: Bool) -> Bool {
    offerExists && !needsAmount
}

/// The label under the QR (PWA `Receive.tsx:993-1001`).
func qrCaption(page: QrPage, invoicePath: InvoicePath, openingFeeSats: UInt64?) -> String {
    if page == .bolt12 { return "Reusable QR code" }
    if invoicePath == .jit, let openingFeeSats {
        return "Setup fee: \(FormatKt.formatBtc(sats: Int64(bitPattern: openingFeeSats)))"
    }
    return "Request money by letting someone scan this QR code"
}

/// The copy sheet's title (PWA `Receive.tsx:1027-1029`).
func copySheetTitle(page: QrPage) -> String {
    page == .bolt12 ? "Reusable payment request" : "Payment request"
}

/// What Copy/Share put on the pasteboard (PWA `Receive.tsx:385-387`): the
/// offer page copies the PWA's `buildBip321Uri({ lno })` form; the unified
/// page copies the bundle's copy-form URI.
func copyValue(page: QrPage, bip321Uri: String, offer: String?) -> String {
    if page == .bolt12, let offer { return "bitcoin:?lno=\(offer)" }
    return bip321Uri
}

/// Header copy icon visibility (PWA `Receive.tsx:642-649`): only over the QR
/// screen — never mid-edit or on any jit-* step.
func headerCopyVisible(hasAddress: Bool, editingAmount: Bool, step: ReceiveStep) -> Bool {
    guard case .display = step else { return false }
    return hasAddress && !editingAmount
}

// MARK: - Expiry countdown (R6 clamp arrives pre-computed in JitInvoice)

/// Seconds until the displayed JIT invoice stops being payable (floored at 0).
func countdownSecondsLeft(expiresAtUnix: UInt64, nowUnixSecs: Int64) -> Int64 {
    max(0, Int64(bitPattern: expiresAtUnix) - nowUnixSecs)
}

/// `m:ss` countdown text, e.g. `Expires in 9:36`.
func countdownText(secondsLeft: Int64) -> String {
    let clamped = max(0, secondsLeft)
    let seconds = String(clamped % 60)
    let padded = seconds.count < 2 ? "0\(seconds)" : seconds
    return "Expires in \(clamped / 60):\(padded)"
}

/// Whether the countdown renders: only over a displayed JIT invoice and never
/// while the user is mid-edit on the numpad (U16 approach: "suppressed
/// mid-edit" — the PWA's numpad overlay has the same effect).
func countdownVisible(step: ReceiveStep, editingAmount: Bool, expiresAtUnix: UInt64?) -> Bool {
    guard case .display(invoicePath: .jit) = step else { return false }
    return expiresAtUnix != nil && !editingAmount
}

/// The expiry flip (PWA `Receive.tsx:319-330`): only a displayed JIT invoice
/// expires into `jit-expired`; every other step is left alone (a payment
/// mid-flight can still settle afterwards — `applyPaymentReceived` supersedes).
func applyExpiryFlip(_ step: ReceiveStep) -> ReceiveStep {
    if case .display(invoicePath: .jit) = step { return .jitExpired }
    return step
}

/// PWA `Receive.tsx:814-818`: if the expiry passes while the user is mid-edit
/// on the numpad, don't yank the numpad away — they land on the expired
/// screen on Cancel instead of on a dead QR.
func showExpiredScreen(step: ReceiveStep, editingAmount: Bool) -> Bool {
    step == .jitExpired && !editingAmount
}

// MARK: - Settlement (PWA Receive.tsx:332-343)

/// Settle the visit from a wallet event: the first `paymentReceived` whose
/// hash matches our displayed invoice's flips to the success screen (from ANY
/// step — success supersedes even `jit-expired`, exactly like the PWA's
/// payment-history watcher). `nil` for everything else.
func applyPaymentReceived(awaitedPaymentHash: String?, event: WalletEvent) -> ReceiveStep? {
    guard let awaitedPaymentHash else { return nil }
    guard case let .paymentReceived(paymentHash, amountMsat, _) = event else { return nil }
    guard paymentHash == awaitedPaymentHash else { return nil }
    // PWA success amount: `match.amountMsat / 1000n` (floor).
    return .received(
        amountSats: UInt64(FormatKt.msatToSatFloor(msat: Int64(bitPattern: amountMsat)))
    )
}

// MARK: - Typed-failure routing

/// Where a Phase A (quote) failure routes.
enum QuoteFailure: Equatable {
    /// The amount is below the LSP's serviceable minimum — render the
    /// below-minimum review variant (PWA `JitPaymentSizeOutOfRangeError`
    /// branch, `Receive.tsx:249-265`). `minPaymentSizeMsat` is the LSP's raw
    /// menu minimum; the session floor from `min_receive_sats` (which adds
    /// the fee headroom, like the PWA's `computeMinReceiveSats`) is the
    /// displayed suggestion.
    case belowMinimum(minPaymentSizeMsat: UInt64)
    /// Anything else: fall back to the on-chain-only QR (PWA `Receive.tsx:266-268`).
    case other
}

/// The core's Lsps2Error::AmountBelowMinimum Display, `selection.rs:83-90`.
private let belowMinimumReason = try! NSRegularExpression(
    pattern: "is below the LSP minimum payment size of (\\d+)msat"
)

/// Classify a `jit_quote` failure. The core folds the LSPS2 taxonomy into
/// `WalletError::Lsps2 { reason }`, so the below-minimum case is recovered
/// from its stable Display copy (`selection.rs`).
func classifyQuoteFailure(_ e: WalletException) -> QuoteFailure {
    guard let lsps2 = e as? WalletException.Lsps2 else { return .other }
    let reason = lsps2.reason
    guard
        let match = belowMinimumReason.firstMatch(
            in: reason,
            range: NSRange(reason.startIndex..., in: reason)
        ),
        let group = Range(match.range(at: 1), in: reason),
        let minMsat = UInt64(reason[group])
    else { return .other }
    return .belowMinimum(minPaymentSizeMsat: minMsat)
}

/// Kotlin exceptions cross the Kotlin/Native bridge as NSError with the
/// original throwable under `KotlinException` (same unwrap as the send flow).
func classifyQuoteFailure(_ error: Error) -> QuoteFailure {
    guard let kotlin = kotlinWalletException(error) else { return .other }
    return classifyQuoteFailure(kotlin)
}

/// Where a Phase B (buy) failure routes.
enum BuyFailure {
    /// The quote went stale client-side BEFORE the buy was issued (the core's
    /// typed `JitReQuoteRequired`, carrying the PWA's "Fee quote expired,
    /// please try again" copy) — re-quote the same LSP, honest-disclosure
    /// style (PWA `Receive.tsx:534-537`).
    case reQuote
    /// Anything else → `jit-error`. The core configures a single LSP (no
    /// `HAS_FALLBACK_LSP`), so the PWA's fallback re-quote branch collapses
    /// to the straight-to-error path (`Receive.tsx:552-556`).
    case error
}

func classifyBuyFailure(_ e: WalletException) -> BuyFailure {
    e is WalletException.JitReQuoteRequired ? .reQuote : .error
}

func classifyBuyFailure(_ error: Error) -> BuyFailure {
    guard let kotlin = kotlinWalletException(error) else { return .error }
    return classifyBuyFailure(kotlin)
}
