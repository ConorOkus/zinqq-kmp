import Foundation
import Shared

/// The pure half of the send flow (U20, F1, R5/R7 UI, R14): the PWA's
/// six-step machine (`Send.tsx`) as immutable step data plus pure routing /
/// gating / derivation functions, ported step-for-step from Android's
/// `SendFlow.kt`. NO Lightning or fee logic lives here — every protocol
/// decision (classification, bounds, fee math, drift) arrives as a core
/// result (`ClassifiedView`, `LnurlPayView`, `FeeEstimate`,
/// `MaxSendEstimate`, typed `WalletException`s) and these functions only
/// route between steps and carry the PWA's copy. `SendController` owns the
/// FFI calls.

/// PWA `Send.tsx:88` — dust floor gate for exact-amount on-chain sends.
let sendMinDustSats: UInt64 = 294

/// PWA `Send.tsx:90` (input `maxLength={2000}`, scanned-input cap too).
let sendInputMaxLength = 2_000

/// PWA `Send.tsx:92` (`MAX_POLL_DURATION_MS`): the outcome wait cap.
let sendOutcomeTimeoutMs: UInt64 = 5 * 60 * 1_000

/// PWA `send-guards.ts:7` (`BALANCE_TOO_LOW_MESSAGE`), verbatim.
let balanceTooLowMessage = "Balance too low to cover fees"

/// PWA `send-guards.ts:10` (`FEES_TOO_HIGH_MESSAGE`), verbatim.
let feesTooHighMessage = "Network fees are too high right now — try again later."

// MARK: - Steps

/// `recipient`: the free-form input screen (Android `SendStep.Input`).
struct SendInputStep: Equatable {
    var error: String? = nil
    var resolving: Bool = false
}

/// `amount`: numpad entry for inputs without an embedded amount. Bounds come
/// exclusively from the core's `lnurl` view (LUD-16 min/max); the on-chain
/// dust floor is applied at submit like the PWA.
struct SendAmountStep: Equatable {
    let target: ClassifiedView
    let rawInput: String
    var lnurl: LnurlPayView? = nil
    var digits: String = ""
    var isSendMax: Bool = false
    var error: String? = nil
    var fetchingInvoice: Bool = false

    var amountSats: UInt64 { digits.isEmpty ? 0 : (UInt64(digits) ?? 0) }
    var minSats: UInt64? { lnurl?.minSats }
    var maxSats: UInt64? { lnurl?.maxSats }
}

/// `ln-review`: BOLT11/BOLT12 review — To + Amount rows.
struct SendLightningReview: Equatable {
    let target: ClassifiedView
    let amountMsat: UInt64
    let recipient: String
    var returnTo: SendAmountStep? = nil
}

/// `oc-review`: Amount / Network fee / Total rows plus the drift banner.
struct SendOnchainReview: Equatable {
    let address: String
    var amountSats: UInt64
    var feeSats: UInt64
    var feeRateSatPerVb: UInt64
    var reserveSats: UInt64
    let isSendMax: Bool
    var amountsUpdated: Bool = false
    var broadcasting: Bool = false
    var returnTo: SendAmountStep? = nil

    var totalSats: UInt64 { amountSats + feeSats }
}

/// One step of the PWA's send machine (`Send.tsx:33-86`), case-for-case with
/// Android's `SendStep` sealed interface.
indirect enum SendStep: Equatable {
    case input(SendInputStep)
    case amount(SendAmountStep)
    case reviewLightning(SendLightningReview)
    case reviewOnchain(SendOnchainReview)
    /// `ln-sending`: dispatched, awaiting the outcome event.
    case dispatching(amountMsat: UInt64)
    /// `oc-success` / `ln-success`: `txid` only for on-chain broadcasts.
    case success(amountSats: UInt64, txid: String?)
    /// `error`: taxonomy message + "Your funds are safe." + optional retry.
    case failure(message: String, retry: SendStep?)
    /// The 5-minute outcome cap fired with the payment still pending. The PWA
    /// offers cancel-as-abandon here; the core exposes no abandon FFI, so
    /// this is a neutral terminal state — the pending history row settles
    /// from events whenever the outcome lands.
    case timedOut(amountMsat: UInt64)

    /// Kotlin-default-args ergonomics for the most common construction.
    static func input(error: String? = nil, resolving: Bool = false) -> SendStep {
        .input(SendInputStep(error: error, resolving: resolving))
    }
}

// MARK: - Decisions

/// What the shell must do next after routing a core result: either move to a
/// `step`, show an `inlineError` on the current screen, or make the named
/// core call and route its result. The controller executes these; tests
/// assert them (Android `SendDecision`).
enum SendDecision: Equatable {
    case step(SendStep)
    case inlineError(String)
    case resolve(input: String)
    case fetchLnurlInvoice(
        lnurl: LnurlPayView,
        amountMsat: UInt64,
        rawInput: String,
        returnTo: SendAmountStep?
    )
    case estimateOnchain(address: String, amountSats: UInt64, returnTo: SendAmountStep?)
    case estimateOnchainMax(address: String, returnTo: SendAmountStep?)
}

// MARK: - Labels

/// PWA `Send.tsx:124-127`: "lnbc1x…q3sdwj" middle truncation.
func truncateInvoice(_ raw: String) -> String {
    raw.count <= 24 ? raw : "\(raw.prefix(12))…\(raw.suffix(8))"
}

/// The ln-review "To" label (PWA `Send.tsx:130-136`, `469-471`, `592-594`):
/// BIP321-wrapped inputs show the invoice's first 10 chars; the amount step
/// always passes the raw input through (user@domain for resolved names, the
/// invoice itself for pasted amountless invoices); fixed-amount resolved
/// names show the name; otherwise description or truncated invoice.
func lightningRecipientLabel(
    view: ClassifiedView,
    rawInput: String,
    fromAmount: Bool
) -> String {
    let invoice = view.bolt11 ?? view.offer
    if let invoice, rawInput.lowercased().hasPrefix("bitcoin:") {
        return String(invoice.prefix(10)) + "…"
    }
    if fromAmount { return rawInput }
    if let invoice, rawInput != invoice { return rawInput }
    if let description = view.description_,
       !description.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty {
        return description
    }
    return truncateInvoice(invoice ?? rawInput)
}

/// The oc-review "To" label (PWA `Send.tsx:1011`).
func onchainRecipientLabel(_ address: String) -> String {
    address.count <= 20 ? address : "\(address.prefix(12))...\(address.suffix(8))"
}

// MARK: - Routing

/// Route a classified (or resolved) input off the recipient screen — the
/// PWA's `processRecipientInput` + `routeResolvedInput` (`Send.tsx:222-528`).
/// Every branch keys off the core's `ClassifiedKind`; the shell adds only
/// the PWA's UI gates (capacity, dust, balance) with its copy.
func routeInput(
    raw: String,
    view: ClassifiedView,
    lnurl: LnurlPayView?,
    lnCapacityMsat: UInt64,
    onchainBalanceSats: UInt64
) -> SendDecision {
    let kind = view.kind

    if kind == .invalid {
        return .inlineError(view.error ?? "Not a valid payment code")
    }

    if kind == .bip353 {
        return .resolve(input: raw)
    }

    if kind == .lnurl {
        guard let meta = lnurl else { return .inlineError("Not a valid payment code") }
        // Fixed-amount LNURL skips the numpad (PWA Send.tsx:323-327).
        if meta.skipAmountEntry {
            return .fetchLnurlInvoice(
                lnurl: meta,
                amountMsat: meta.minSendableMsat,
                rawInput: raw,
                returnTo: nil
            )
        }
        return .step(.amount(SendAmountStep(target: view, rawInput: raw, lnurl: meta)))
    }

    if kind == .onchain {
        let address = view.address ?? ""
        guard let embedded = view.amountSats?.uint64Value else {
            return .step(.amount(SendAmountStep(target: view, rawInput: raw)))
        }
        // PWA Send.tsx:405-408 — dust gate before any estimate.
        if embedded < sendMinDustSats {
            return .inlineError(
                "Amount must be at least "
                    + "\(FormatKt.formatBtc(sats: Int64(sendMinDustSats))) (dust limit)"
            )
        }
        // PWA Send.tsx:440-443.
        if embedded > onchainBalanceSats {
            return .inlineError("Amount exceeds available on-chain balance")
        }
        return .estimateOnchain(address: address, amountSats: embedded, returnTo: nil)
    }

    // BOLT11 / BOLT12 (the classifier's remaining kinds).
    if let embeddedMsat = view.amountMsat?.uint64Value {
        // PWA Send.tsx:472-475.
        if embeddedMsat > lnCapacityMsat {
            return .inlineError("Not enough funds")
        }
        return .step(
            .reviewLightning(
                SendLightningReview(
                    target: view,
                    amountMsat: embeddedMsat,
                    recipient: lightningRecipientLabel(
                        view: view, rawInput: raw, fromAmount: false
                    ),
                    returnTo: nil
                )
            )
        )
    }
    return .step(.amount(SendAmountStep(target: view, rawInput: raw)))
}

// MARK: - Amount step

/// Numpad key press on the amount step (PWA `Send.tsx:177-181`).
func reduceAmountKey(_ step: SendAmountStep, key: NumpadInput) -> SendAmountStep {
    var next = step
    next.digits = NumpadReducer.reduce(step.digits, key)
    next.isSendMax = false
    next.error = nil
    return next
}

/// The Lightning "₿X available" prefill (PWA `Send.tsx:208-219`): the
/// unified total capped at the LNURL max when one exists.
func lnAvailablePrefillSats(unifiedTotalSats: UInt64, maxSats: UInt64?) -> UInt64 {
    if let maxSats, maxSats < unifiedTotalSats { return maxSats }
    return unifiedTotalSats
}

/// PWA `use-unified-balance`: on-chain balance + floored Lightning sats.
func unifiedTotalSats(onchainBalanceSats: UInt64, lightningMsat: UInt64) -> UInt64 {
    onchainBalanceSats + UInt64(FormatKt.msatToSatFloor(msat: Int64(bitPattern: lightningMsat)))
}

/// Amount-step Next (PWA `handleAmountNext`, `Send.tsx:549-606`): LNURL
/// bounds gate, then per-kind routing with the dust/balance/capacity gates.
func submitAmount(
    _ step: SendAmountStep,
    lnCapacityMsat: UInt64,
    onchainBalanceSats: UInt64
) -> SendDecision {
    let amountSats = step.amountSats
    if amountSats == 0 { return .inlineError("") }

    // LNURL bounds (PWA Send.tsx:554-561).
    if let min = step.minSats, amountSats < min {
        return .inlineError(
            "Minimum amount is \(FormatKt.formatBtc(sats: Int64(bitPattern: min)))"
        )
    }
    if let max = step.maxSats, amountSats > max {
        return .inlineError(
            "Maximum amount is \(FormatKt.formatBtc(sats: Int64(bitPattern: max)))"
        )
    }

    let kind = step.target.kind

    if kind == .lnurl {
        guard let lnurl = step.lnurl else { return .inlineError("Not a valid payment code") }
        return .fetchLnurlInvoice(
            lnurl: lnurl,
            amountMsat: amountSats * 1_000,
            rawInput: step.rawInput,
            returnTo: step
        )
    }

    if kind == .bolt11 || kind == .bolt12 {
        let amountMsat = amountSats * 1_000
        if amountMsat > lnCapacityMsat {
            return .inlineError("Not enough funds")
        }
        return .step(
            .reviewLightning(
                SendLightningReview(
                    target: step.target,
                    amountMsat: amountMsat,
                    recipient: lightningRecipientLabel(
                        view: step.target,
                        rawInput: step.rawInput,
                        fromAmount: true
                    ),
                    returnTo: step
                )
            )
        )
    }

    if kind == .onchain {
        let address = step.target.address ?? ""
        // Send-max skips the dust gate — the estimate's guards own it
        // (PWA Send.tsx:402-408, 412-437).
        if step.isSendMax {
            return .estimateOnchainMax(address: address, returnTo: step)
        }
        if amountSats < sendMinDustSats {
            return .inlineError(
                "Amount must be at least "
                    + "\(FormatKt.formatBtc(sats: Int64(sendMinDustSats))) (dust limit)"
            )
        }
        if amountSats > onchainBalanceSats {
            return .inlineError("Amount exceeds available on-chain balance")
        }
        return .estimateOnchain(address: address, amountSats: amountSats, returnTo: step)
    }

    return .inlineError("Not a valid payment code")
}

// MARK: - Review derivation (R7)

/// Review for a core-fetched-and-validated LNURL invoice (PWA
/// `fetchAndRouteInvoice`, `Send.tsx:249-283`): the core already
/// re-classified the invoice and enforced the amount/description-hash
/// commitments (KTD-6).
func lnurlInvoiceReview(
    invoice: ClassifiedView,
    requestedMsat: UInt64,
    rawInput: String,
    returnTo: SendAmountStep?
) -> SendLightningReview {
    SendLightningReview(
        target: invoice,
        amountMsat: invoice.amountMsat?.uint64Value ?? requestedMsat,
        recipient: lightningRecipientLabel(view: invoice, rawInput: rawInput, fromAmount: true),
        returnTo: returnTo
    )
}

/// Exact-amount on-chain review rows from the core's estimate (R7).
func onchainReview(
    address: String,
    amountSats: UInt64,
    estimate: FeeEstimate,
    returnTo: SendAmountStep?
) -> SendOnchainReview {
    SendOnchainReview(
        address: address,
        amountSats: amountSats,
        feeSats: estimate.feeSats,
        feeRateSatPerVb: estimate.feeRateSatPerVb,
        reserveSats: 0,
        isSendMax: false,
        returnTo: returnTo
    )
}

/// Send-max review rows from the core's drain estimate (R7, AE6).
func onchainMaxReview(
    address: String,
    estimate: MaxSendEstimate,
    returnTo: SendAmountStep?
) -> SendOnchainReview {
    SendOnchainReview(
        address: address,
        amountSats: estimate.amountSats,
        feeSats: estimate.feeSats,
        feeRateSatPerVb: estimate.feeRateSatPerVb,
        reserveSats: estimate.reserveSats,
        isSendMax: true,
        returnTo: returnTo
    )
}

/// R5 drift guard re-render (PWA `showRefreshedReview`, `Send.tsx:678-687`):
/// replace the reviewed figures with the fresh estimate and raise the
/// "Amounts were updated" banner so the user re-verifies before confirming.
func refreshedMaxReview(
    _ review: SendOnchainReview,
    fresh: MaxSendEstimate
) -> SendOnchainReview {
    var next = review
    next.amountSats = fresh.amountSats
    next.feeSats = fresh.feeSats
    next.feeRateSatPerVb = fresh.feeRateSatPerVb
    next.reserveSats = fresh.reserveSats
    next.amountsUpdated = true
    next.broadcasting = false
    return next
}

/// Drift re-render for the exact-amount path (amount is fixed; fees moved).
func refreshedExactReview(
    _ review: SendOnchainReview,
    fresh: FeeEstimate
) -> SendOnchainReview {
    var next = review
    next.feeSats = fresh.feeSats
    next.feeRateSatPerVb = fresh.feeRateSatPerVb
    next.amountsUpdated = true
    next.broadcasting = false
    return next
}

// MARK: - Outcome events (F1)

/// The events that settle a dispatched Lightning payment (F1).
func isPaymentOutcome(_ event: WalletEvent) -> Bool {
    switch event {
    case .paymentSuccessful, .paymentFailed:
        return true
    default:
        return false
    }
}

/// Settle the dispatch step from an outcome event; `nil` for events that are
/// not this payment's outcome. The core exposes no FFI to derive our
/// dispatch's payment hash in the shell, so the first outcome after dispatch
/// is taken as ours (one send is in flight per screen).
func applyOutcome(amountMsat: UInt64, event: WalletEvent) -> SendStep? {
    switch event {
    case .paymentSuccessful:
        return .success(
            amountSats: UInt64(FormatKt.msatToSatCeil(msat: Int64(bitPattern: amountMsat))),
            txid: nil
        )
    case let .paymentFailed(reason):
        return .failure(message: reason, retry: nil)
    default:
        return nil
    }
}

/// The 5-minute cap fired with no outcome event (see `SendStep.timedOut`).
func outcomeTimedOut(amountMsat: UInt64) -> SendStep {
    .timedOut(amountMsat: amountMsat)
}

// MARK: - Error copy (PWA taxonomy)

/// Typed core errors to the PWA's user-facing copy — Android's
/// `walletErrorMessage` mapping ported check-for-check. The mapping mirrors
/// the core's own `Display` strings — which already carry the PWA's taxonomy
/// verbatim (U6/U8) — plus the PWA's `classifyEstimateError` rewrites
/// (`Send.tsx:95-121`).
func walletErrorMessage(_ e: WalletException) -> String {
    switch e {
    case is WalletException.OnchainFeesTooHigh:
        return feesTooHighMessage
    case is WalletException.OnchainBalanceTooLow:
        return balanceTooLowMessage
    case is WalletException.WrongAddressNetwork:
        return "This address is for a different Bitcoin network"
    case is WalletException.InvalidAddress:
        return "Invalid Bitcoin address"
    case let e as WalletException.OnchainInsufficientFunds:
        return "Insufficient funds after reserving "
            + "\(FormatKt.formatBtc(sats: Int64(bitPattern: e.reserveSats))) "
            + "for Lightning channel safety"
    case is WalletException.OnchainAmountBelowDust:
        return "Amount is below the minimum for this address"
    case is WalletException.OnchainAmountChanged:
        return "Send amount changed since review"
    case let e as WalletException.OnchainSendFailed:
        return e.detail
    case let e as WalletException.ResolveFailed:
        return e.detail
    case let e as WalletException.SendFailed:
        return e.reason
    case let e as WalletException.InvalidInvoice:
        return "invalid bolt11 invoice: \(e.detail)"
    case is WalletException.InvoiceExpired:
        return "the invoice is expired"
    case let e as WalletException.WrongNetwork:
        return "the invoice is for the \(e.network) network, this wallet only pays bitcoin "
            + "(mainnet) invoices"
    case let e as WalletException.InvalidOffer:
        return "invalid bolt12 offer: \(e.detail)"
    case is WalletException.OfferExpired:
        return "the offer is expired"
    case is WalletException.OfferWrongNetwork:
        return "the offer is for a different network, this wallet only pays bitcoin offers"
    case is WalletException.AmountlessInvoice:
        return "Amount is required for invoices without an embedded amount"
    case is WalletException.AmountOverrideNotAllowed:
        return "an amount override is only allowed for requests without an embedded amount"
    case is WalletException.DuplicatePayment:
        return "a payment for this invoice is already pending"
    case is WalletException.NotRunning:
        return "the node is not running"
    default:
        if let message = e.message, !message.isEmpty { return message }
        return "Something went wrong"
    }
}

/// Kotlin exceptions cross the Kotlin/Native bridge as NSError with the
/// original throwable under `KotlinException` (same unwrap as
/// `WalletModel.isFencedError`); non-Kotlin failures fall back to their own
/// description.
func walletErrorMessage(_ error: Error) -> String {
    if let kotlin = kotlinWalletException(error) {
        return walletErrorMessage(kotlin)
    }
    if error is SendPortError { return "the node is not running" }
    let fallback = (error as NSError).localizedDescription
    return fallback.isEmpty ? "Something went wrong" : fallback
}

/// Whether a confirm-time failure routes back to the amount step with an
/// inline message instead of the error screen (PWA `returnToAmountWithError`,
/// `Send.tsx:653-672`: the friendly balance/fee guard messages only).
func isAmountStepGuardError(_ e: WalletException) -> Bool {
    e is WalletException.OnchainBalanceTooLow || e is WalletException.OnchainFeesTooHigh
}

func isAmountStepGuardError(_ error: Error) -> Bool {
    guard let kotlin = kotlinWalletException(error) else { return false }
    return isAmountStepGuardError(kotlin)
}

/// The typed `WalletException` carried inside a bridged NSError, if any.
func kotlinWalletException(_ error: Error) -> WalletException? {
    (error as NSError).userInfo["KotlinException"] as? WalletException
}
