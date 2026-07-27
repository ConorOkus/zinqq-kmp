package zinqq.app.screens.receive

import kotlin.coroutines.cancellation.CancellationException
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Job
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.update
import kotlinx.coroutines.launch
import uniffi.wallet_core.Event
import uniffi.wallet_core.JitInvoice
import uniffi.wallet_core.JitQuote
import uniffi.wallet_core.ReceiveBundle
import zinqq.app.screens.send.walletErrorMessage
import zinqq.spike.NumpadKey
import zinqq.spike.msatToSatCeil
import zinqq.spike.numpadDigitReducer

/**
 * The receive flow's window onto the wallet (U16, R14): every call is a thin
 * passthrough to the core FFI — the capacity decision, floor computation,
 * quote/buy protocol, and the invoice-expiry clamp all happen in Rust.
 * [WalletHolder][zinqq.app.WalletHolder] implements this; tests can fake it.
 */
interface ReceivePort {
    suspend fun receiveBundle(amountMsat: ULong?): ReceiveBundle
    suspend fun jitQuote(amountMsat: ULong): JitQuote
    suspend fun jitAccept(quoteToken: ULong, amountMsat: ULong): JitInvoice

    /** R6: the live JIT floor; `refresh = true` is the one get_info per visit. */
    suspend fun minReceiveSats(refresh: Boolean): ULong

    /** Sum of usable inbound capacity, for the typing-time JIT gate (AE4). */
    suspend fun usableInboundMsat(): ULong

    /** The core's `build_bip321_uri` (copy form) — re-composes around a JIT invoice. */
    suspend fun buildUnifiedUri(address: String, amountSats: ULong?, invoice: String?): String

    /** The core's live event stream ([Event.PaymentReceived] settles the visit). */
    val walletEvents: Flow<Event>
}

/**
 * Everything the Receive screen renders (U16). [step] is the PWA's machine;
 * the flat fields mirror the PWA's `useState` cluster (`Receive.tsx:72-93`).
 */
data class ReceiveUiState(
    val loading: Boolean = true,
    /** Fatal entry failure (PWA's onchain-error screen, `Receive.tsx:592-602`). */
    val loadError: String? = null,
    val address: String? = null,
    /** Copy/share form (address uppercased, rest untouched). */
    val bip321Uri: String = "",
    /** QR form (whole URI uppercased for alphanumeric mode, `Receive.tsx:640`). */
    val qrValue: String = "",
    val offer: String? = null,
    val offerQrValue: String? = null,
    /** PWA `Receive.tsx:290`: amounted standard invoice failed; QR still renders. */
    val invoiceError: String? = null,
    /** The displayed invoice's hash — what [applyPaymentReceived] awaits. */
    val paymentHash: String? = null,
    /** JIT only: the agreed fee for the under-QR caption (`Receive.tsx:999`). */
    val openingFeeSats: ULong? = null,
    /** JIT only: UNIX seconds when the displayed invoice stops being payable. */
    val expiresAtUnix: ULong? = null,
    /** No usable channels → JIT required → amount required (`Receive.tsx:112-116`). */
    val needsAmount: Boolean = true,
    /** The session numpad floor (core-computed: live when fetched, else static). */
    val floorSats: ULong = 3_000uL,
    /** Usable inbound capacity snapshot for the typing-time gate. */
    val usableInboundMsat: ULong = 0uL,
    val step: ReceiveStep = ReceiveStep.Display(),
    val editingAmount: Boolean = false,
    val amountDigits: String = "",
    val confirmedDigits: String = "",
) {
    val editingAmountSats: ULong get() = amountDigits.toULongOrNull() ?: 0uL
    val confirmedAmountSats: ULong get() = confirmedDigits.toULongOrNull() ?: 0uL
}

/**
 * Drives [ReceiveStep] through the core (U16): executes the pure layer's
 * decisions, owns the coroutines and the expiry timer, and routes typed
 * failures through [classifyQuoteFailure]/[classifyBuyFailure]. One instance
 * per Receive visit.
 */
class ReceiveController(
    private val port: ReceivePort,
    private val scope: CoroutineScope,
    private val nowUnixSecs: () -> Long = { System.currentTimeMillis() / 1_000 },
) {
    private val _state = MutableStateFlow(ReceiveUiState())
    val state: StateFlow<ReceiveUiState> = _state.asStateFlow()

    private var requestJob: Job? = null
    private var expiryJob: Job? = null
    private var started = false

    /** Screen entry: floor fetch + the amountless default bundle + settlement watch. */
    fun start() {
        if (started) return
        started = true

        // Success watcher (PWA Receive.tsx:332-343): subscribe for the whole
        // visit; the first PaymentReceived matching our displayed invoice
        // settles the screen from any step.
        scope.launch {
            port.walletEvents.collect { event ->
                _state.update { s ->
                    applyPaymentReceived(s.paymentHash, event)
                        ?.let { s.copy(step = it) }
                        ?: s
                }
            }
        }

        requestJob = scope.launch {
            try {
                val inbound = port.usableInboundMsat()
                // R6: one live-floor get_info per visit, and only when it can
                // matter — the gate binds only if usable inbound capacity is
                // itself below the static floor (PWA Receive.tsx:158-176).
                // Failure degrades to the core's static fallback silently.
                if (inbound < _state.value.floorSats * 1_000uL) {
                    runCatching { port.minReceiveSats(refresh = true) }
                }
                val bundle = port.receiveBundle(null)
                _state.update { s ->
                    applyBundle(s, bundle).copy(
                        loading = false,
                        usableInboundMsat = inbound,
                        needsAmount = bundle.needsJit,
                        // Start on the numpad when amount is required
                        // (PWA Receive.tsx:137-143).
                        editingAmount = bundle.needsJit,
                        step = ReceiveStep.Display(
                            if (bundle.bolt11 != null) {
                                InvoicePath.STANDARD
                            } else {
                                InvoicePath.NONE
                            },
                        ),
                    )
                }
            } catch (e: CancellationException) {
                throw e
            } catch (e: Exception) {
                // The send flow's shared typed-error copy (iOS
                // ReceiveController parity); the PWA's onchain-error fallback
                // covers anything the mapping leaves blank.
                _state.update {
                    it.copy(
                        loading = false,
                        loadError = walletErrorMessage(e)
                            .ifBlank { "Failed to load wallet" },
                    )
                }
            }
        }
    }

    fun onNumpadKey(key: NumpadKey) {
        _state.update {
            it.copy(amountDigits = numpadDigitReducer(it.amountDigits, key, RECEIVE_MAX_DIGITS))
        }
    }

    /** Numpad Next (PWA `handleConfirmAmount`, `Receive.tsx:425-439`). */
    fun confirmAmount() {
        val s = _state.value
        val decision = confirmAmount(s.editingAmountSats, s.usableInboundMsat, s.floorSats)
        if (decision !is ConfirmDecision.Request) return
        _state.update {
            it.copy(
                confirmedDigits = it.amountDigits,
                editingAmount = false,
                // Flip to the quoting skeleton in the same update as the
                // commit so no stale QR frame renders (PWA Receive.tsx:430-435).
                step = if (decision.presentQuoting) ReceiveStep.Quoting else it.step,
            )
        }
        requestBundle(decision.amountSats)
    }

    /** PWA `handleCancelAmount` (`Receive.tsx:441-445`). */
    fun cancelAmount() {
        _state.update { it.copy(amountDigits = it.confirmedDigits, editingAmount = false) }
    }

    /** PWA `handleEditAmount` (`Receive.tsx:459-462`). */
    fun editAmount() {
        _state.update { it.copy(amountDigits = it.confirmedDigits, editingAmount = true) }
    }

    /** PWA `handleRemoveAmount` (`Receive.tsx:447-457`): back to the amountless QR. */
    fun removeAmount() {
        _state.update {
            it.copy(
                amountDigits = "",
                confirmedDigits = "",
                // Stay on the numpad when the amount is mandatory.
                editingAmount = it.needsAmount,
            )
        }
        requestBundle(0uL)
    }

    /**
     * Review/expired/error Back (PWA `handleReviewBack`, `Receive.tsx:560-568`):
     * abandon the quote, restore the numpad with the amount preserved, and
     * regenerate the amountless default behind it.
     */
    fun backFromReview() {
        _state.update {
            it.copy(
                amountDigits = it.confirmedDigits,
                confirmedDigits = "",
                step = ReceiveStep.Display(InvoicePath.NONE),
                editingAmount = true,
            )
        }
        requestBundle(0uL)
    }

    /** Expired/error retry (PWA `handleErrorRetry`): re-run the flow at the amount. */
    fun retryRequest() {
        val amountSats = _state.value.confirmedAmountSats
        if (amountSats == 0uL) {
            backFromReview()
            return
        }
        _state.update { it.copy(step = ReceiveStep.Quoting) }
        requestBundle(amountSats)
    }

    /** Review CTA (PWA `handleGenerateInvoice`, `Receive.tsx:503-558`): Phase B. */
    fun generateInvoice() {
        val review = _state.value.step as? ReceiveStep.JitReview ?: return
        requestJob?.cancel()
        requestJob = scope.launch {
            _state.update { it.copy(step = ReceiveStep.Buying) }
            try {
                val invoice = port.jitAccept(review.quote.quoteToken, review.quote.amountMsat)
                val address = _state.value.address.orEmpty()
                val uri = port.buildUnifiedUri(address, review.amountSats, invoice.bolt11)
                _state.update {
                    it.copy(
                        bip321Uri = uri,
                        qrValue = uri.uppercase(),
                        paymentHash = invoice.paymentHash,
                        // PWA fee display ceils: (openingFeeMsat + 999n) / 1000n.
                        openingFeeSats =
                            msatToSatCeil(invoice.openingFeeMsat.toLong()).toULong(),
                        expiresAtUnix = invoice.expiresAtUnix,
                        invoiceError = null,
                        step = ReceiveStep.Display(InvoicePath.JIT),
                    )
                }
                scheduleExpiry(invoice.expiresAtUnix)
            } catch (e: CancellationException) {
                throw e
            } catch (e: Exception) {
                when (classifyBuyFailure(e)) {
                    // Stale quote, raised BEFORE any buy — re-quote the same
                    // LSP set (PWA Receive.tsx:534-537).
                    BuyFailure.RE_QUOTE -> runQuote(review.amountSats, reQuote = true)
                    BuyFailure.ERROR -> _state.update { it.copy(step = ReceiveStep.JitError) }
                }
            }
        }
    }

    // ------------------------------------------------------------------

    /**
     * The PWA's flow-driver effect (`Receive.tsx:194-298`): fetch a bundle for
     * the confirmed amount; the core's `needs_jit` routes to the standard QR
     * or into Phase A.
     */
    private fun requestBundle(amountSats: ULong) {
        requestJob?.cancel()
        requestJob = scope.launch {
            val amountMsat = if (amountSats > 0uL) amountSats * 1_000uL else null
            val bundle = try {
                port.receiveBundle(amountMsat)
            } catch (e: CancellationException) {
                throw e
            } catch (e: Exception) {
                // PWA Receive.tsx:285-293: the on-chain QR keeps rendering;
                // an amounted failure surfaces the invoice error copy.
                _state.update {
                    it.copy(
                        invoiceError =
                            if (amountSats > 0uL) "Failed to create Lightning invoice" else null,
                        paymentHash = null,
                        openingFeeSats = null,
                        expiresAtUnix = null,
                        step = ReceiveStep.Display(InvoicePath.NONE),
                    )
                }
                return@launch
            }
            if (bundle.needsJit && amountMsat != null) {
                _state.update { applyBundle(it, bundle).copy(step = ReceiveStep.Quoting) }
                runQuote(amountSats, reQuote = false)
            } else {
                _state.update {
                    applyBundle(it, bundle).copy(
                        step = ReceiveStep.Display(
                            if (bundle.bolt11 != null) {
                                InvoicePath.STANDARD
                            } else {
                                InvoicePath.NONE
                            },
                        ),
                    )
                }
            }
        }
    }

    /** JIT Phase A (PWA `Receive.tsx:220-274` + `reQuote`, `Receive.tsx:473-501`). */
    private suspend fun runQuote(amountSats: ULong, reQuote: Boolean) {
        try {
            val quote = port.jitQuote(amountSats * 1_000uL)
            _state.update {
                it.copy(step = ReceiveStep.JitReview(amountSats, quote, quoteUpdated = reQuote))
            }
        } catch (e: CancellationException) {
            throw e
        } catch (e: Exception) {
            when (val failure = classifyQuoteFailure(e)) {
                is QuoteFailure.BelowMinimum -> {
                    // Sync the numpad gate to the freshest observed menu and
                    // render the below-minimum review (PWA Receive.tsx:249-265).
                    // The suggested minimum is the core's headroom-adjusted
                    // floor (its computeMinReceiveSats), never the raw menu min.
                    val refreshed =
                        runCatching { port.minReceiveSats(refresh = true) }.getOrDefault(0uL)
                    _state.update {
                        val displayMin = if (refreshed > 0uL) refreshed else it.floorSats
                        it.copy(
                            floorSats = displayMin,
                            step = ReceiveStep.JitBelowMinimum(amountSats, displayMin),
                        )
                    }
                }

                QuoteFailure.Other -> _state.update {
                    if (reQuote) {
                        // Re-quote failure → jit-error (PWA Receive.tsx:494-498).
                        it.copy(step = ReceiveStep.JitError)
                    } else {
                        // Phase A failure → on-chain-only QR (PWA Receive.tsx:266-268).
                        it.copy(step = ReceiveStep.Display(InvoicePath.NONE))
                    }
                }
            }
        }
    }

    /** The expiry flip (PWA `Receive.tsx:319-330`), guarded by [applyExpiryFlip]. */
    private fun scheduleExpiry(expiresAtUnix: ULong) {
        expiryJob?.cancel()
        expiryJob = scope.launch {
            delay(countdownSecondsLeft(expiresAtUnix, nowUnixSecs()) * 1_000)
            _state.update { it.copy(step = applyExpiryFlip(it.step)) }
        }
    }

    /** Fold a fresh core bundle into the state (URIs, offer, hash, floor). */
    private fun applyBundle(s: ReceiveUiState, bundle: ReceiveBundle): ReceiveUiState = s.copy(
        address = bundle.address,
        bip321Uri = bundle.bip321Uri,
        qrValue = bundle.qrValue,
        offer = bundle.offer,
        offerQrValue = bundle.offerQrValue,
        invoiceError = bundle.invoiceError,
        paymentHash = bundle.paymentHash,
        openingFeeSats = null,
        expiresAtUnix = null,
        floorSats = bundle.minReceiveSats,
    )
}
