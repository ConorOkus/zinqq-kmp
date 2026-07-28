package zinqq.app.screens.send

import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.CoroutineStart
import kotlinx.coroutines.Job
import kotlinx.coroutines.async
import kotlin.coroutines.cancellation.CancellationException
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.launch
import kotlinx.coroutines.withTimeoutOrNull
import uniffi.wallet_core.ClassifiedKind
import uniffi.wallet_core.ClassifiedView
import uniffi.wallet_core.Event
import uniffi.wallet_core.FeeEstimate
import uniffi.wallet_core.LnurlPayView
import uniffi.wallet_core.MaxSendEstimate
import uniffi.wallet_core.ResolvedView
import uniffi.wallet_core.WalletException
import zinqq.spike.NumpadKey

/**
 * The send flow's window onto the wallet (U15, R14): every call is a thin
 * passthrough to the core FFI — classification, resolution, fee estimates,
 * and dispatch all happen in Rust. [WalletHolder][zinqq.app.WalletHolder]
 * implements this; tests can fake it.
 */
interface SendPort {
    suspend fun classify(input: String): ClassifiedView
    suspend fun resolve(input: String): ResolvedView
    suspend fun fetchLnurlInvoice(lnurl: LnurlPayView, amountMsat: ULong): ClassifiedView
    suspend fun sendBolt11(bolt11: String, amountMsat: ULong?)
    suspend fun payOffer(offer: String, amountMsat: ULong?)
    suspend fun estimateOnchainFee(address: String, amountSats: ULong): FeeEstimate
    suspend fun estimateMaxSendable(address: String): MaxSendEstimate
    suspend fun sendOnchain(
        address: String,
        amountSats: ULong,
        expectedAmountSats: ULong,
        expectedFeeSats: ULong,
    ): String
    suspend fun sendOnchainMax(
        address: String,
        expectedAmountSats: ULong,
        expectedFeeSats: ULong,
    ): String

    /** The core's live event stream (payment outcomes arrive here, F1). */
    val walletEvents: Flow<Event>

    /** Snapshot balances for the PWA's UI gates (capacity / available). */
    fun lightningCapacityMsat(): ULong
    fun onchainBalanceSats(): ULong
}

/**
 * Drives [SendStep] through the core (U15): executes the pure layer's
 * [SendDecision]s, owns the coroutines, and maps typed failures through
 * [walletErrorMessage]. One instance per Send visit.
 */
class SendController(
    private val port: SendPort,
    private val scope: CoroutineScope,
    private val outcomeTimeoutMs: Long = SEND_OUTCOME_TIMEOUT_MS,
) {
    private val _step = MutableStateFlow<SendStep>(SendStep.Input())
    val step: StateFlow<SendStep> = _step.asStateFlow()

    private var job: Job? = null

    /** Recipient-screen Continue / paste / scanned input (same path, R13). */
    fun submitInput(raw: String) {
        val trimmed = raw.trim()
        if (trimmed.isEmpty()) {
            _step.value = SendStep.Input(error = "Enter a payment request or address")
            return
        }
        if (trimmed.length > SEND_INPUT_MAX_LENGTH) {
            _step.value = SendStep.Input(error = "Scanned input is too long")
            return
        }
        job?.cancel()
        job = scope.launch {
            val view = try {
                port.classify(trimmed)
            } catch (e: CancellationException) {
                throw e
            } catch (e: Exception) {
                _step.value = SendStep.Input(error = walletErrorMessage(e))
                return@launch
            }
            execute(
                routeInput(
                    trimmed, view, lnurl = null,
                    lnCapacityMsat = port.lightningCapacityMsat(),
                    onchainBalanceSats = port.onchainBalanceSats(),
                ),
                trimmed,
            )
        }
    }

    /** Abort an in-flight BIP353/LNURL resolution (PWA `Send.tsx:1180-1183`). */
    fun abortResolve() {
        job?.cancel()
        job = null
        _step.value = SendStep.Input()
    }

    fun onNumpadKey(key: NumpadKey) {
        val amount = _step.value as? SendStep.Amount ?: return
        _step.value = reduceAmountKey(amount, key)
    }

    /**
     * On-chain Max (PWA `handleOnchainSendAll`): the exact prefill comes
     * from the core's drain estimate — the shell never does reserve math.
     */
    fun setOnchainSendMax() {
        val amount = _step.value as? SendStep.Amount ?: return
        val address = amount.target.address ?: return
        job?.cancel()
        job = scope.launch {
            try {
                val estimate = port.estimateMaxSendable(address)
                (_step.value as? SendStep.Amount)?.let { current ->
                    _step.value = current.copy(
                        digits = estimate.amountSats.toString(),
                        isSendMax = true,
                        error = null,
                    )
                }
            } catch (e: CancellationException) {
                throw e
            } catch (e: Exception) {
                (_step.value as? SendStep.Amount)?.let { current ->
                    _step.value = current.copy(error = walletErrorMessage(e))
                }
            }
        }
    }

    /** Lightning "₿X available" prefill (PWA `handleApproxSendMax`). */
    fun setLightningAvailable() {
        val amount = _step.value as? SendStep.Amount ?: return
        val total = unifiedTotalSats(port.onchainBalanceSats(), port.lightningCapacityMsat())
        val prefill = lnAvailablePrefillSats(total, amount.maxSats)
        if (prefill == 0uL) return
        _step.value = amount.copy(digits = prefill.toString(), error = null)
    }

    fun submitAmountStep() {
        val amount = _step.value as? SendStep.Amount ?: return
        if (amount.amountSats == 0uL) return
        job?.cancel()
        job = scope.launch {
            execute(
                submitAmount(
                    amount,
                    lnCapacityMsat = port.lightningCapacityMsat(),
                    onchainBalanceSats = port.onchainBalanceSats(),
                ),
                amount.rawInput,
            )
        }
    }

    /** Review back (PWA `handleReviewBack`): amount step if it existed, else input. */
    fun backFromReview() {
        val back = when (val current = _step.value) {
            is SendStep.ReviewLightning -> current.returnTo
            is SendStep.ReviewOnchain -> current.returnTo
            else -> null
        }
        _step.value = back?.copy(error = null, fetchingInvoice = false) ?: SendStep.Input()
    }

    /** Amount-step header back → recipient screen. */
    fun backToInput() {
        _step.value = SendStep.Input()
    }

    /** Result-screen Try Again (PWA `Send.tsx:930-941`). */
    fun retry(step: SendStep) {
        _step.value = step
    }

    /** Confirm Send on the Lightning review (PWA `handleLnConfirm`). */
    fun confirmLightning() {
        val review = _step.value as? SendStep.ReviewLightning ?: return
        job?.cancel()
        job = scope.launch {
            // Our dispatch's hash, when the core named it before dispatch
            // (BOLT11 and LNURL-fetched invoices; null for BOLT12 offers).
            // Outcomes carrying any other hash are not ours — a previous
            // send that outlived its 5-minute cap is still in flight and
            // must not settle this one (F1).
            val dispatchHash = review.paymentHash
            _step.value = SendStep.Dispatching(review.amountMsat, dispatchHash)
            // Subscribe BEFORE dispatch so an instant outcome cannot be missed.
            val outcome = async(start = CoroutineStart.UNDISPATCHED) {
                withTimeoutOrNull(outcomeTimeoutMs) {
                    port.walletEvents.first { isPaymentOutcome(it, dispatchHash) }
                }
            }
            try {
                // The amount override is REQUIRED for amountless requests and
                // REJECTED otherwise (core U6 matrix) — key off the embedded
                // amount exactly like the PWA (Send.tsx:748-758).
                val override =
                    if (review.target.amountMsat == null) review.amountMsat else null
                when (review.target.kind) {
                    ClassifiedKind.BOLT12 ->
                        port.payOffer(requireNotNull(review.target.offer), override)
                    else ->
                        port.sendBolt11(requireNotNull(review.target.bolt11), override)
                }
            } catch (e: CancellationException) {
                throw e
            } catch (e: Exception) {
                outcome.cancel()
                _step.value = SendStep.Failure(message = walletErrorMessage(e))
                return@launch
            }
            val event = outcome.await()
            val dispatching = SendStep.Dispatching(review.amountMsat, dispatchHash)
            _step.value = event?.let { applyOutcome(dispatching, it) }
                ?: outcomeTimedOut(dispatching)
        }
    }

    /** Confirm Send on the on-chain review (PWA `handleOcConfirm`, R5/R7). */
    fun confirmOnchain() {
        val review = _step.value as? SendStep.ReviewOnchain ?: return
        if (review.broadcasting) return
        job?.cancel()
        job = scope.launch {
            _step.value = review.copy(broadcasting = true)
            try {
                val txid = if (review.isSendMax) {
                    port.sendOnchainMax(
                        address = review.address,
                        expectedAmountSats = review.amountSats,
                        expectedFeeSats = review.feeSats,
                    )
                } else {
                    port.sendOnchain(
                        address = review.address,
                        amountSats = review.amountSats,
                        expectedAmountSats = review.amountSats,
                        expectedFeeSats = review.feeSats,
                    )
                }
                _step.value = SendStep.Success(amountSats = review.amountSats, txid = txid)
            } catch (e: CancellationException) {
                throw e
            } catch (e: WalletException.OnchainAmountChanged) {
                // R5 drift guard: nothing was signed or broadcast — re-run
                // the estimate and re-render the review with the "Amounts
                // were updated" banner (PWA Send.tsx:678-716).
                refreshReviewAfterDrift(review)
            } catch (e: Exception) {
                routeConfirmFailure(review, e)
            }
        }
    }

    private suspend fun refreshReviewAfterDrift(review: SendStep.ReviewOnchain) {
        try {
            _step.value = if (review.isSendMax) {
                refreshedMaxReview(review, port.estimateMaxSendable(review.address))
            } else {
                refreshedExactReview(
                    review,
                    port.estimateOnchainFee(review.address, review.amountSats),
                )
            }
        } catch (e: CancellationException) {
            throw e
        } catch (e: Exception) {
            routeConfirmFailure(review, e)
        }
    }

    private fun routeConfirmFailure(review: SendStep.ReviewOnchain, e: Exception) {
        val message = walletErrorMessage(e)
        _step.value = if (isAmountStepGuardError(e)) {
            // PWA returnToAmountWithError: back to the amount step (or the
            // recipient screen) with the friendly inline message.
            review.returnTo?.copy(error = message, fetchingInvoice = false)
                ?: SendStep.Input(error = message)
        } else {
            SendStep.Failure(message = message, retry = review.copy(broadcasting = false))
        }
    }

    /** Execute a routing decision from the pure layer. */
    private suspend fun execute(decision: SendDecision, rawInput: String) {
        when (decision) {
            is SendDecision.Step -> _step.value = decision.step

            is SendDecision.InlineError -> showInlineError(decision.message)

            is SendDecision.Resolve -> {
                _step.value = SendStep.Input(resolving = true)
                val resolved = try {
                    port.resolve(decision.input)
                } catch (e: CancellationException) {
                    throw e
                } catch (e: Exception) {
                    _step.value = SendStep.Input(error = walletErrorMessage(e))
                    return
                }
                _step.value = SendStep.Input()
                execute(
                    routeInput(
                        decision.input, resolved.classified, resolved.lnurl,
                        lnCapacityMsat = port.lightningCapacityMsat(),
                        onchainBalanceSats = port.onchainBalanceSats(),
                    ),
                    rawInput,
                )
            }

            is SendDecision.FetchLnurlInvoice -> {
                markBusy(decision.returnTo)
                try {
                    val invoice = port.fetchLnurlInvoice(decision.lnurl, decision.amountMsat)
                    _step.value = lnurlInvoiceReview(
                        invoice = invoice,
                        requestedMsat = decision.amountMsat,
                        rawInput = decision.rawInput,
                        returnTo = decision.returnTo,
                    )
                } catch (e: CancellationException) {
                    throw e
                } catch (e: Exception) {
                    failBusy(decision.returnTo, walletErrorMessage(e))
                }
            }

            is SendDecision.EstimateOnchain -> {
                markBusy(decision.returnTo)
                try {
                    val estimate =
                        port.estimateOnchainFee(decision.address, decision.amountSats)
                    _step.value = onchainReview(
                        address = decision.address,
                        amountSats = decision.amountSats,
                        estimate = estimate,
                        returnTo = decision.returnTo,
                    )
                } catch (e: CancellationException) {
                    throw e
                } catch (e: Exception) {
                    failBusy(decision.returnTo, walletErrorMessage(e))
                }
            }

            is SendDecision.EstimateOnchainMax -> {
                markBusy(decision.returnTo)
                try {
                    val estimate = port.estimateMaxSendable(decision.address)
                    _step.value = onchainMaxReview(
                        address = decision.address,
                        estimate = estimate,
                        returnTo = decision.returnTo,
                    )
                } catch (e: CancellationException) {
                    throw e
                } catch (e: Exception) {
                    failBusy(decision.returnTo, walletErrorMessage(e))
                }
            }
        }
    }

    private fun showInlineError(message: String) {
        _step.value = when (val current = _step.value) {
            is SendStep.Input -> current.copy(error = message, resolving = false)
            is SendStep.Amount -> current.copy(error = message, fetchingInvoice = false)
            else -> SendStep.Input(error = message)
        }
    }

    /** Show a spinner on whichever screen initiated a slow core call. */
    private fun markBusy(returnTo: SendStep.Amount?) {
        _step.value = returnTo?.copy(fetchingInvoice = true, error = null)
            ?: SendStep.Input(resolving = true)
    }

    private fun failBusy(returnTo: SendStep.Amount?, message: String) {
        _step.value = returnTo?.copy(fetchingInvoice = false, error = message)
            ?: SendStep.Input(error = message)
    }
}
