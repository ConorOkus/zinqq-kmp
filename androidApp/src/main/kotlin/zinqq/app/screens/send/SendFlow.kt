package zinqq.app.screens.send

import uniffi.wallet_core.ClassifiedKind
import uniffi.wallet_core.ClassifiedView
import uniffi.wallet_core.Event
import uniffi.wallet_core.FeeEstimate
import uniffi.wallet_core.LnurlPayView
import uniffi.wallet_core.MaxSendEstimate
import uniffi.wallet_core.WalletException
import zinqq.spike.NumpadKey
import zinqq.spike.formatBtc
import zinqq.spike.msatToSatCeil
import zinqq.spike.msatToSatFloor
import zinqq.spike.numpadDigitReducer

/**
 * The pure half of the send flow (U15, F1, R5/R7 UI, R14): the PWA's
 * six-step machine (`Send.tsx`) as immutable step data plus pure routing /
 * gating / derivation functions. NO Lightning or fee logic lives here — every
 * protocol decision (classification, bounds, fee math, drift) arrives as a
 * core result ([ClassifiedView], [LnurlPayView], [FeeEstimate],
 * [MaxSendEstimate], typed [WalletException]s) and these functions only route
 * between steps and carry the PWA's copy. [SendController] owns the FFI calls.
 */

/** PWA `Send.tsx:88` — dust floor gate for exact-amount on-chain sends. */
const val SEND_MIN_DUST_SATS: ULong = 294uL

/** PWA `Send.tsx:90` (input `maxLength={2000}`, scanned-input cap too). */
const val SEND_INPUT_MAX_LENGTH = 2000

/** PWA `Send.tsx:92` (`MAX_POLL_DURATION_MS`): the outcome wait cap. */
const val SEND_OUTCOME_TIMEOUT_MS = 5L * 60 * 1_000

/** PWA `send-guards.ts:7` (`BALANCE_TOO_LOW_MESSAGE`), verbatim. */
const val BALANCE_TOO_LOW_MESSAGE = "Balance too low to cover fees"

/** PWA `send-guards.ts:10` (`FEES_TOO_HIGH_MESSAGE`), verbatim. */
const val FEES_TOO_HIGH_MESSAGE = "Network fees are too high right now — try again later."

/** One step of the PWA's send machine (`Send.tsx:33-86`). */
sealed interface SendStep {
    /** `recipient`: the free-form input screen. */
    data class Input(
        val error: String? = null,
        val resolving: Boolean = false,
    ) : SendStep

    /**
     * `amount`: numpad entry for inputs without an embedded amount. Bounds
     * come exclusively from the core's [lnurl] view (LUD-16 min/max); the
     * on-chain dust floor is applied at submit like the PWA.
     */
    data class Amount(
        val target: ClassifiedView,
        val rawInput: String,
        val lnurl: LnurlPayView? = null,
        val digits: String = "",
        val isSendMax: Boolean = false,
        val error: String? = null,
        val fetchingInvoice: Boolean = false,
    ) : SendStep {
        val amountSats: ULong get() = if (digits.isEmpty()) 0uL else digits.toULong()
        val minSats: ULong? get() = lnurl?.minSats
        val maxSats: ULong? get() = lnurl?.maxSats
    }

    /** `ln-review`: BOLT11/BOLT12 review — To + Amount rows. */
    data class ReviewLightning(
        val target: ClassifiedView,
        val amountMsat: ULong,
        val recipient: String,
        val returnTo: Amount? = null,
    ) : SendStep

    /** `oc-review`: Amount / Network fee / Total rows plus the drift banner. */
    data class ReviewOnchain(
        val address: String,
        val amountSats: ULong,
        val feeSats: ULong,
        val feeRateSatPerVb: ULong,
        val reserveSats: ULong,
        val isSendMax: Boolean,
        val amountsUpdated: Boolean = false,
        val broadcasting: Boolean = false,
        val returnTo: Amount? = null,
    ) : SendStep {
        val totalSats: ULong get() = amountSats + feeSats
    }

    /** `ln-sending`: dispatched, awaiting the outcome event. */
    data class Dispatching(val amountMsat: ULong) : SendStep

    /** `oc-success` / `ln-success`: [txid] only for on-chain broadcasts. */
    data class Success(val amountSats: ULong, val txid: String? = null) : SendStep

    /** `error`: taxonomy message + "Your funds are safe." + optional retry. */
    data class Failure(val message: String, val retry: SendStep? = null) : SendStep

    /**
     * The 5-minute outcome cap fired with the payment still pending. The PWA
     * offers cancel-as-abandon here; the core exposes no abandon FFI, so this
     * is a neutral terminal state — the pending history row settles from
     * events whenever the outcome lands.
     */
    data class TimedOut(val amountMsat: ULong) : SendStep
}

/**
 * What the shell must do next after routing a core result: either move to a
 * [Step], show an inline [InlineError] on the current screen, or make the
 * named core call and route its result. The controller executes these; tests
 * assert them.
 */
sealed interface SendDecision {
    data class Step(val step: SendStep) : SendDecision
    data class InlineError(val message: String) : SendDecision
    data class Resolve(val input: String) : SendDecision
    data class FetchLnurlInvoice(
        val lnurl: LnurlPayView,
        val amountMsat: ULong,
        val rawInput: String,
        val returnTo: SendStep.Amount?,
    ) : SendDecision
    data class EstimateOnchain(
        val address: String,
        val amountSats: ULong,
        val returnTo: SendStep.Amount?,
    ) : SendDecision
    data class EstimateOnchainMax(
        val address: String,
        val returnTo: SendStep.Amount?,
    ) : SendDecision
}

/** PWA `Send.tsx:124-127`: "lnbc1x…q3sdwj" middle truncation. */
fun truncateInvoice(raw: String): String =
    if (raw.length <= 24) raw else "${raw.take(12)}…${raw.takeLast(8)}"

/**
 * The ln-review "To" label (PWA `Send.tsx:130-136`, `469-471`, `592-594`):
 * BIP321-wrapped inputs show the invoice's first 10 chars; the amount step
 * always passes the raw input through (user@domain for resolved names, the
 * invoice itself for pasted amountless invoices); fixed-amount resolved
 * names show the name; otherwise description or truncated invoice.
 */
fun lightningRecipientLabel(
    view: ClassifiedView,
    rawInput: String,
    fromAmount: Boolean,
): String {
    val invoice = view.bolt11 ?: view.offer
    return when {
        invoice != null && rawInput.lowercase().startsWith("bitcoin:") -> invoice.take(10) + "…"
        fromAmount -> rawInput
        invoice != null && rawInput != invoice -> rawInput
        else -> view.description?.takeIf { it.isNotBlank() }
            ?: truncateInvoice(invoice ?: rawInput)
    }
}

/** The oc-review "To" label (PWA `Send.tsx:1011`). */
fun onchainRecipientLabel(address: String): String =
    if (address.length <= 20) address else "${address.take(12)}...${address.takeLast(8)}"

/**
 * Route a classified (or resolved) input off the recipient screen — the
 * PWA's `processRecipientInput` + `routeResolvedInput` (`Send.tsx:222-528`).
 * Every branch keys off the core's [ClassifiedKind]; the shell adds only the
 * PWA's UI gates (capacity, dust, balance) with its copy.
 */
fun routeInput(
    raw: String,
    view: ClassifiedView,
    lnurl: LnurlPayView?,
    lnCapacityMsat: ULong,
    onchainBalanceSats: ULong,
): SendDecision = when (view.kind) {
    ClassifiedKind.INVALID ->
        SendDecision.InlineError(view.error ?: "Not a valid payment code")

    ClassifiedKind.BIP353 -> SendDecision.Resolve(raw)

    ClassifiedKind.LNURL -> {
        val meta = lnurl
        when {
            meta == null -> SendDecision.InlineError("Not a valid payment code")
            // Fixed-amount LNURL skips the numpad (PWA Send.tsx:323-327).
            meta.skipAmountEntry ->
                SendDecision.FetchLnurlInvoice(meta, meta.minSendableMsat, raw, returnTo = null)
            else -> SendDecision.Step(
                SendStep.Amount(target = view, rawInput = raw, lnurl = meta),
            )
        }
    }

    ClassifiedKind.ONCHAIN -> {
        val address = view.address.orEmpty()
        val embedded = view.amountSats
        when {
            embedded == null ->
                SendDecision.Step(SendStep.Amount(target = view, rawInput = raw))
            // PWA Send.tsx:405-408 — dust gate before any estimate.
            embedded < SEND_MIN_DUST_SATS -> SendDecision.InlineError(
                "Amount must be at least ${formatBtc(SEND_MIN_DUST_SATS.toLong())} (dust limit)",
            )
            // PWA Send.tsx:440-443.
            embedded > onchainBalanceSats ->
                SendDecision.InlineError("Amount exceeds available on-chain balance")
            else -> SendDecision.EstimateOnchain(address, embedded, returnTo = null)
        }
    }

    ClassifiedKind.BOLT11, ClassifiedKind.BOLT12 -> {
        val embeddedMsat = view.amountMsat
        if (embeddedMsat != null) {
            // PWA Send.tsx:472-475.
            if (embeddedMsat > lnCapacityMsat) {
                SendDecision.InlineError("Not enough funds")
            } else {
                SendDecision.Step(
                    SendStep.ReviewLightning(
                        target = view,
                        amountMsat = embeddedMsat,
                        recipient = lightningRecipientLabel(view, raw, fromAmount = false),
                    ),
                )
            }
        } else {
            SendDecision.Step(SendStep.Amount(target = view, rawInput = raw))
        }
    }
}

/** Numpad key press on the amount step (PWA `Send.tsx:177-181`). */
fun reduceAmountKey(step: SendStep.Amount, key: NumpadKey): SendStep.Amount =
    step.copy(
        digits = numpadDigitReducer(step.digits, key),
        isSendMax = false,
        error = null,
    )

/**
 * The Lightning "₿X available" prefill (PWA `Send.tsx:208-219`): the unified
 * total capped at the LNURL max when one exists.
 */
fun lnAvailablePrefillSats(unifiedTotalSats: ULong, maxSats: ULong?): ULong =
    if (maxSats != null && maxSats < unifiedTotalSats) maxSats else unifiedTotalSats

/** PWA `use-unified-balance`: on-chain balance + floored Lightning sats. */
fun unifiedTotalSats(onchainBalanceSats: ULong, lightningMsat: ULong): ULong =
    onchainBalanceSats + msatToSatFloor(lightningMsat.toLong()).toULong()

/**
 * Amount-step Next (PWA `handleAmountNext`, `Send.tsx:549-606`): LNURL
 * bounds gate, then per-kind routing with the dust/balance/capacity gates.
 */
fun submitAmount(
    step: SendStep.Amount,
    lnCapacityMsat: ULong,
    onchainBalanceSats: ULong,
): SendDecision {
    val amountSats = step.amountSats
    if (amountSats == 0uL) return SendDecision.InlineError("")

    // LNURL bounds (PWA Send.tsx:554-561).
    val min = step.minSats
    if (min != null && amountSats < min) {
        return SendDecision.InlineError("Minimum amount is ${formatBtc(min.toLong())}")
    }
    val max = step.maxSats
    if (max != null && amountSats > max) {
        return SendDecision.InlineError("Maximum amount is ${formatBtc(max.toLong())}")
    }

    return when (step.target.kind) {
        ClassifiedKind.LNURL -> SendDecision.FetchLnurlInvoice(
            lnurl = requireNotNull(step.lnurl),
            amountMsat = amountSats * 1_000uL,
            rawInput = step.rawInput,
            returnTo = step,
        )

        ClassifiedKind.BOLT11, ClassifiedKind.BOLT12 -> {
            val amountMsat = amountSats * 1_000uL
            if (amountMsat > lnCapacityMsat) {
                SendDecision.InlineError("Not enough funds")
            } else {
                SendDecision.Step(
                    SendStep.ReviewLightning(
                        target = step.target,
                        amountMsat = amountMsat,
                        recipient = lightningRecipientLabel(
                            step.target,
                            step.rawInput,
                            fromAmount = true,
                        ),
                        returnTo = step,
                    ),
                )
            }
        }

        ClassifiedKind.ONCHAIN -> {
            val address = step.target.address.orEmpty()
            when {
                // Send-max skips the dust gate — the estimate's guards own it
                // (PWA Send.tsx:402-408, 412-437).
                step.isSendMax -> SendDecision.EstimateOnchainMax(address, returnTo = step)
                amountSats < SEND_MIN_DUST_SATS -> SendDecision.InlineError(
                    "Amount must be at least ${formatBtc(SEND_MIN_DUST_SATS.toLong())} (dust limit)",
                )
                amountSats > onchainBalanceSats ->
                    SendDecision.InlineError("Amount exceeds available on-chain balance")
                else -> SendDecision.EstimateOnchain(address, amountSats, returnTo = step)
            }
        }

        else -> SendDecision.InlineError("Not a valid payment code")
    }
}

/**
 * Review for a core-fetched-and-validated LNURL invoice (PWA
 * `fetchAndRouteInvoice`, `Send.tsx:249-283`): the core already re-classified
 * the invoice and enforced the amount/description-hash commitments (KTD-6).
 */
fun lnurlInvoiceReview(
    invoice: ClassifiedView,
    requestedMsat: ULong,
    rawInput: String,
    returnTo: SendStep.Amount?,
): SendStep.ReviewLightning = SendStep.ReviewLightning(
    target = invoice,
    amountMsat = invoice.amountMsat ?: requestedMsat,
    recipient = lightningRecipientLabel(invoice, rawInput, fromAmount = true),
    returnTo = returnTo,
)

/** Exact-amount on-chain review rows from the core's estimate (R7). */
fun onchainReview(
    address: String,
    amountSats: ULong,
    estimate: FeeEstimate,
    returnTo: SendStep.Amount?,
): SendStep.ReviewOnchain = SendStep.ReviewOnchain(
    address = address,
    amountSats = amountSats,
    feeSats = estimate.feeSats,
    feeRateSatPerVb = estimate.feeRateSatPerVb,
    reserveSats = 0uL,
    isSendMax = false,
    returnTo = returnTo,
)

/** Send-max review rows from the core's drain estimate (R7, AE6). */
fun onchainMaxReview(
    address: String,
    estimate: MaxSendEstimate,
    returnTo: SendStep.Amount?,
): SendStep.ReviewOnchain = SendStep.ReviewOnchain(
    address = address,
    amountSats = estimate.amountSats,
    feeSats = estimate.feeSats,
    feeRateSatPerVb = estimate.feeRateSatPerVb,
    reserveSats = estimate.reserveSats,
    isSendMax = true,
    returnTo = returnTo,
)

/**
 * R5 drift guard re-render (PWA `showRefreshedReview`, `Send.tsx:678-687`):
 * replace the reviewed figures with the fresh estimate and raise the
 * "Amounts were updated" banner so the user re-verifies before confirming.
 */
fun refreshedMaxReview(
    review: SendStep.ReviewOnchain,
    fresh: MaxSendEstimate,
): SendStep.ReviewOnchain = review.copy(
    amountSats = fresh.amountSats,
    feeSats = fresh.feeSats,
    feeRateSatPerVb = fresh.feeRateSatPerVb,
    reserveSats = fresh.reserveSats,
    amountsUpdated = true,
    broadcasting = false,
)

/** Drift re-render for the exact-amount path (amount is fixed; fees moved). */
fun refreshedExactReview(
    review: SendStep.ReviewOnchain,
    fresh: FeeEstimate,
): SendStep.ReviewOnchain = review.copy(
    feeSats = fresh.feeSats,
    feeRateSatPerVb = fresh.feeRateSatPerVb,
    amountsUpdated = true,
    broadcasting = false,
)

/** The events that settle a dispatched Lightning payment (F1). */
fun isPaymentOutcome(event: Event): Boolean =
    event is Event.PaymentSuccessful || event is Event.PaymentFailed

/**
 * Settle the dispatch step from an outcome event; `null` for events that
 * are not this payment's outcome. The core exposes no FFI to derive our
 * dispatch's payment hash in the shell, so the first outcome after dispatch
 * is taken as ours (one send is in flight per screen).
 */
fun applyOutcome(step: SendStep.Dispatching, event: Event): SendStep? = when (event) {
    is Event.PaymentSuccessful ->
        SendStep.Success(amountSats = msatToSatCeil(step.amountMsat.toLong()).toULong())
    is Event.PaymentFailed -> SendStep.Failure(message = event.reason)
    else -> null
}

/** The 5-minute cap fired with no outcome event (see [SendStep.TimedOut]). */
fun outcomeTimedOut(step: SendStep.Dispatching): SendStep.TimedOut =
    SendStep.TimedOut(step.amountMsat)

/**
 * Typed core errors to the PWA's user-facing copy. The generated
 * [WalletException] messages are field dumps (`"detail=…"`), so the mapping
 * mirrors the core's own `Display` strings — which already carry the PWA's
 * taxonomy verbatim (U6/U8) — plus the PWA's `classifyEstimateError`
 * rewrites (`Send.tsx:95-121`).
 */
fun walletErrorMessage(e: Throwable): String = when (e) {
    is WalletException.OnchainFeesTooHigh -> FEES_TOO_HIGH_MESSAGE
    is WalletException.OnchainBalanceTooLow -> BALANCE_TOO_LOW_MESSAGE
    is WalletException.WrongAddressNetwork -> "This address is for a different Bitcoin network"
    is WalletException.InvalidAddress -> "Invalid Bitcoin address"
    is WalletException.OnchainInsufficientFunds ->
        "Insufficient funds after reserving ${formatBtc(e.reserveSats.toLong())} " +
            "for Lightning channel safety"
    is WalletException.OnchainAmountBelowDust ->
        "Amount is below the minimum for this address"
    is WalletException.OnchainAmountChanged -> "Send amount changed since review"
    is WalletException.OnchainSendFailed -> e.detail
    is WalletException.ResolveFailed -> e.detail
    is WalletException.SendFailed -> e.reason
    is WalletException.InvalidInvoice -> "invalid bolt11 invoice: ${e.detail}"
    is WalletException.InvoiceExpired -> "the invoice is expired"
    is WalletException.WrongNetwork ->
        "the invoice is for the ${e.network} network, this wallet only pays bitcoin " +
            "(mainnet) invoices"
    is WalletException.InvalidOffer -> "invalid bolt12 offer: ${e.detail}"
    is WalletException.OfferExpired -> "the offer is expired"
    is WalletException.OfferWrongNetwork ->
        "the offer is for a different network, this wallet only pays bitcoin offers"
    is WalletException.AmountlessInvoice ->
        "Amount is required for invoices without an embedded amount"
    is WalletException.AmountOverrideNotAllowed ->
        "an amount override is only allowed for requests without an embedded amount"
    is WalletException.DuplicatePayment -> "a payment for this invoice is already pending"
    is WalletException.NotRunning -> "the node is not running"
    else -> e.message?.takeIf { it.isNotBlank() } ?: "Something went wrong"
}

/**
 * Whether a confirm-time failure routes back to the amount step with an
 * inline message instead of the error screen (PWA `returnToAmountWithError`,
 * `Send.tsx:653-672`: the friendly balance/fee guard messages only).
 */
fun isAmountStepGuardError(e: Throwable): Boolean =
    e is WalletException.OnchainBalanceTooLow || e is WalletException.OnchainFeesTooHigh
