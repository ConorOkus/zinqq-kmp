package zinqq.app.screens.receive

import uniffi.wallet_core.ChannelView
import uniffi.wallet_core.Event
import uniffi.wallet_core.JitQuote
import uniffi.wallet_core.ReceiveBundle
import uniffi.wallet_core.WalletException
import zinqq.spike.formatBtc
import zinqq.spike.msatToSatCeil
import zinqq.spike.msatToSatFloor

/**
 * The pure half of the receive flow (U16, F2, R6 UI, R12): the PWA's
 * `Receive.tsx` state machine as immutable step data plus pure gating /
 * derivation functions. NO liquidity logic lives here — the capacity decision
 * (`needs_jit`), the live floor, quote fees, and the invoice-expiry clamp all
 * arrive as core results ([ReceiveBundle], [JitQuote], `JitInvoice`, typed
 * [WalletException]s); these functions only route between steps and carry the
 * PWA's copy. [ReceiveController] owns the FFI calls.
 */

/** PWA `Receive.tsx:30` (`MAX_DIGITS`): receive amounts cap at 8 digits. */
const val RECEIVE_MAX_DIGITS = 8

/** PWA `Receive.tsx:401`: "Copied!" feedback duration. */
const val COPY_FEEDBACK_MS = 2_000L

/** Which invoice the unified QR currently embeds (PWA `InvoicePath`). */
enum class InvoicePath { NONE, STANDARD, JIT }

/** The snap pager's two pages (PWA `QrPage`). */
enum class QrPage { UNIFIED, BOLT12 }

/** One step of the PWA's receive machine (`Receive.tsx:43-66`). */
sealed interface ReceiveStep {
    /** `ready`: the QR screen; [invoicePath] says what the URI embeds. */
    data class Display(val invoicePath: InvoicePath = InvoicePath.NONE) : ReceiveStep

    /** `jit-quoting`: Phase A in flight — review skeleton renders. */
    data object Quoting : ReceiveStep

    /**
     * `jit-review` (kind `commit`): fee disclosure before committing
     * LSP-side liquidity. Fee rows ceil like the PWA (`Receive.tsx:732`).
     */
    data class JitReview(
        val amountSats: ULong,
        val quote: JitQuote,
        /** The PWA's `quoteStatus: 'updated'` — this quote replaced a stale one. */
        val quoteUpdated: Boolean = false,
    ) : ReceiveStep {
        val setupFeeSats: ULong get() = msatToSatCeil(quote.openingFeeMsat.toLong()).toULong()
        val youReceiveSats: ULong get() = amountSats - setupFeeSats
    }

    /** `jit-review` (kind `below-minimum`): disabled CTA + suggested floor. */
    data class JitBelowMinimum(
        val amountSats: ULong,
        val displayMinSats: ULong,
    ) : ReceiveStep

    /** `jit-buying`: Phase B in flight. */
    data object Buying : ReceiveStep

    /** `jit-error`: quote/buy failed; retry re-runs Phase A. */
    data object JitError : ReceiveStep

    /** `jit-expired`: the displayed JIT invoice outlived its quote validity. */
    data object JitExpired : ReceiveStep

    /** `success`: an inbound payment matching our invoice settled. */
    data class Received(val amountSats: ULong) : ReceiveStep
}

// --- capacity + floor gating (AE4) ---

/** PWA `Receive.tsx:33-39`: sum of inbound capacity across usable channels. */
fun usableInboundMsat(channels: List<ChannelView>): ULong =
    channels.filter { it.usable }.fold(0uL) { acc, ch -> acc + ch.inboundMsat }

/**
 * Whether the amount being typed would require a JIT channel
 * (PWA `Receive.tsx:121-122` `editingNeedsJit`).
 */
fun editingNeedsJit(usableInboundMsat: ULong, amountSats: ULong): Boolean =
    usableInboundMsat < amountSats * 1_000uL

/**
 * AE4 / PWA `Receive.tsx:133-134`: block advancing past the numpad for a JIT
 * receive below the floor the LSP can service. On-chain / in-capacity
 * receives are unaffected.
 */
fun belowJitMinimum(needsJit: Boolean, amountSats: ULong, floorSats: ULong): Boolean =
    needsJit && amountSats > 0uL && amountSats < floorSats

/** PWA `Receive.tsx:927` (`nextDisabled`), inverted. */
fun numpadNextEnabled(amountSats: ULong, belowMinimum: Boolean): Boolean =
    amountSats > 0uL && !belowMinimum

/** PWA `Receive.tsx:928`: 'Request' on the mandatory first entry, else 'Done'. */
fun numpadCtaLabel(needsAmount: Boolean, confirmedAmountSats: ULong): String =
    if (needsAmount && confirmedAmountSats <= 0uL) "Request" else "Done"

/** PWA `Receive.tsx:918-921`: the below-floor numpad alert (AE4 copy). */
fun minimumAlertText(floorSats: ULong): String = "Minimum ${formatBtc(floorSats.toLong())}"

/**
 * Numpad Next routing (PWA `handleConfirmAmount`, `Receive.tsx:425-439`):
 * blocked below the JIT floor (defense in depth — the button is also
 * disabled); otherwise commit, flipping straight to the quoting skeleton
 * when the amount needs JIT so no stale QR frame renders.
 */
sealed interface ConfirmDecision {
    data object Blocked : ConfirmDecision
    data class Request(val amountSats: ULong, val presentQuoting: Boolean) : ConfirmDecision
}

fun confirmAmount(
    amountSats: ULong,
    usableInboundMsat: ULong,
    floorSats: ULong,
): ConfirmDecision {
    val needsJit = editingNeedsJit(usableInboundMsat, amountSats)
    if (belowJitMinimum(needsJit, amountSats, floorSats)) return ConfirmDecision.Blocked
    return ConfirmDecision.Request(
        amountSats = amountSats,
        presentQuoting = needsJit && amountSats > 0uL,
    )
}

// --- pager + captions ---

/**
 * Pager eligibility (PWA `Receive.tsx:372` `showBolt12`): the offer page
 * exists only when the core produced an offer (which it does only with ≥1
 * usable channel) AND the visit is not the no-channel mandatory-amount case.
 */
fun showBolt12Page(offerExists: Boolean, needsAmount: Boolean): Boolean =
    offerExists && !needsAmount

/** The label under the QR (PWA `Receive.tsx:993-1001`). */
fun qrCaption(page: QrPage, invoicePath: InvoicePath, openingFeeSats: ULong?): String = when {
    page == QrPage.BOLT12 -> "Reusable QR code"
    invoicePath == InvoicePath.JIT && openingFeeSats != null ->
        "Setup fee: ${formatBtc(openingFeeSats.toLong())}"
    else -> "Request money by letting someone scan this QR code"
}

/** The copy sheet's title (PWA `Receive.tsx:1027-1029`). */
fun copySheetTitle(page: QrPage): String =
    if (page == QrPage.BOLT12) "Reusable payment request" else "Payment request"

/**
 * What Copy/Share put on the pasteboard (PWA `Receive.tsx:385-387`): the
 * offer page copies the PWA's `buildBip321Uri({ lno })` form; the unified
 * page copies the bundle's copy-form URI.
 */
fun copyValue(page: QrPage, bip321Uri: String, offer: String?): String =
    if (page == QrPage.BOLT12 && offer != null) "bitcoin:?lno=$offer" else bip321Uri

/**
 * Header copy icon visibility (PWA `Receive.tsx:642-649`): only over the QR
 * screen — never mid-edit or on any jit-* step.
 */
fun headerCopyVisible(hasAddress: Boolean, editingAmount: Boolean, step: ReceiveStep): Boolean =
    hasAddress && !editingAmount && step is ReceiveStep.Display

// --- expiry countdown (R6 clamp arrives pre-computed in JitInvoice) ---

/** Seconds until the displayed JIT invoice stops being payable (floored at 0). */
fun countdownSecondsLeft(expiresAtUnix: ULong, nowUnixSecs: Long): Long =
    (expiresAtUnix.toLong() - nowUnixSecs).coerceAtLeast(0)

/** `m:ss` countdown text, e.g. `Expires in 9:36`. */
fun countdownText(secondsLeft: Long): String {
    val clamped = secondsLeft.coerceAtLeast(0)
    return "Expires in ${clamped / 60}:${(clamped % 60).toString().padStart(2, '0')}"
}

/**
 * Whether the countdown renders: only over a displayed JIT invoice and never
 * while the user is mid-edit on the numpad (U16 approach: "suppressed
 * mid-edit" — the PWA's numpad overlay has the same effect).
 */
fun countdownVisible(step: ReceiveStep, editingAmount: Boolean, expiresAtUnix: ULong?): Boolean =
    step is ReceiveStep.Display &&
        step.invoicePath == InvoicePath.JIT &&
        expiresAtUnix != null &&
        !editingAmount

/**
 * The expiry flip (PWA `Receive.tsx:319-330`): only a displayed JIT invoice
 * expires into `jit-expired`; every other step is left alone (a payment
 * mid-flight can still settle afterwards — [applyPaymentReceived] supersedes).
 */
fun applyExpiryFlip(step: ReceiveStep): ReceiveStep =
    if (step is ReceiveStep.Display && step.invoicePath == InvoicePath.JIT) {
        ReceiveStep.JitExpired
    } else {
        step
    }

/**
 * PWA `Receive.tsx:814-818`: if the expiry passes while the user is mid-edit
 * on the numpad, don't yank the numpad away — they land on the expired
 * screen on Cancel instead of on a dead QR.
 */
fun showExpiredScreen(step: ReceiveStep, editingAmount: Boolean): Boolean =
    step is ReceiveStep.JitExpired && !editingAmount

// --- settlement (PWA Receive.tsx:332-343) ---

/**
 * Settle the visit from a wallet event: the first [Event.PaymentReceived]
 * whose hash matches our displayed invoice's flips to the success screen
 * (from ANY step — success supersedes even `jit-expired`, exactly like the
 * PWA's payment-history watcher). `null` for everything else.
 */
fun applyPaymentReceived(awaitedPaymentHash: String?, event: Event): ReceiveStep.Received? {
    if (awaitedPaymentHash == null) return null
    if (event !is Event.PaymentReceived) return null
    if (event.paymentHash != awaitedPaymentHash) return null
    // PWA success amount: `match.amountMsat / 1000n` (floor).
    return ReceiveStep.Received(msatToSatFloor(event.amountMsat.toLong()).toULong())
}

// --- typed-failure routing ---

/** Where a Phase A (quote) failure routes. */
sealed interface QuoteFailure {
    /**
     * The amount is below the LSP's serviceable minimum — render the
     * below-minimum review variant (PWA `JitPaymentSizeOutOfRangeError`
     * branch, `Receive.tsx:249-265`). [minPaymentSizeMsat] is the LSP's raw
     * menu minimum; the session floor from `min_receive_sats` (which adds
     * the fee headroom, like the PWA's `computeMinReceiveSats`) is the
     * displayed suggestion.
     */
    data class BelowMinimum(val minPaymentSizeMsat: ULong) : QuoteFailure

    /** Anything else: fall back to the on-chain-only QR (PWA `Receive.tsx:266-268`). */
    data object Other : QuoteFailure
}

// The core's Lsps2Error::AmountBelowMinimum Display, `selection.rs:83-90`.
private val BELOW_MINIMUM_REASON =
    Regex("""is below the LSP minimum payment size of (\d+)msat""")

/**
 * Classify a `jit_quote` failure. The core folds the LSPS2 taxonomy into
 * `WalletError::Lsps2 { reason }`, so the below-minimum case is recovered
 * from its stable Display copy (`selection.rs`).
 */
fun classifyQuoteFailure(e: Throwable): QuoteFailure {
    val reason = (e as? WalletException.Lsps2)?.reason ?: return QuoteFailure.Other
    val match = BELOW_MINIMUM_REASON.find(reason) ?: return QuoteFailure.Other
    val minMsat = match.groupValues[1].toULongOrNull() ?: return QuoteFailure.Other
    return QuoteFailure.BelowMinimum(minMsat)
}

/** Where a Phase B (buy) failure routes. */
enum class BuyFailure {
    /**
     * The quote went stale client-side BEFORE the buy was issued (the core's
     * typed `JitReQuoteRequired`, carrying the PWA's "Fee quote expired,
     * please try again" copy) — re-quote the same LSP, honest-disclosure
     * style (PWA `Receive.tsx:534-537`).
     */
    RE_QUOTE,

    /**
     * Anything else → `jit-error`. The core configures a single LSP (no
     * `HAS_FALLBACK_LSP`), so the PWA's fallback re-quote branch collapses
     * to the straight-to-error path (`Receive.tsx:552-556`).
     */
    ERROR,
}

fun classifyBuyFailure(e: Throwable): BuyFailure =
    if (e is WalletException.JitReQuoteRequired) BuyFailure.RE_QUOTE else BuyFailure.ERROR
