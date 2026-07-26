package zinqq.spike.android

import uniffi.wallet_core.Event

/** The invoice currently on screen, straight off [Event.InvoiceReady]. */
data class InvoiceUi(
    val bolt11: String,
    val expiryUnixSecs: ULong,
)

/** Immutable screen state; only [reduce] and balance refreshes produce new values. */
data class UiState(
    val nodeRunning: Boolean = false,
    val balanceMsat: ULong = 0uL,
    val onchainSats: ULong = 0uL,
    val currentInvoice: InvoiceUi? = null,
    val lastOutcome: String? = null,
    val syncBanner: String? = null,
)

/**
 * Pure event-to-state reduction, unit-testable without Android or a wallet
 * instance. No Lightning logic lives here (R4): event fields pass through to
 * display, and the [Event.PaymentReceived] balance bump is optimistic
 * bookkeeping that the next authoritative `balances()` refresh overwrites.
 */
fun reduce(state: UiState, event: Event): UiState =
    when (event) {
        is Event.NodeStarted -> state.copy(nodeRunning = true)
        is Event.NodeStopped -> state.copy(nodeRunning = false)
        is Event.SyncFailed -> state.copy(syncBanner = "Chain sync failed — retrying")
        is Event.SyncCompleted -> state.copy(syncBanner = null)
        is Event.InvoiceReady ->
            state.copy(currentInvoice = InvoiceUi(event.bolt11, event.expiryUnixSecs))
        is Event.PaymentReceived ->
            state.copy(
                balanceMsat = state.balanceMsat + event.amountMsat,
                currentInvoice = null,
                lastOutcome = buildString {
                    append("Received ${event.amountMsat / 1_000uL} sats")
                    event.skimmedFeeMsat?.let { append(" (LSP fee ${it / 1_000uL} sats)") }
                },
            )
        is Event.PaymentSuccessful -> state.copy(lastOutcome = "Payment sent")
        is Event.PaymentFailed -> state.copy(lastOutcome = "Payment failed: ${event.reason}")
        is Event.ChannelPending -> state.copy(lastOutcome = "JIT channel opening")
        is Event.ChannelReady -> state.copy(lastOutcome = "Channel ready")
        is Event.Lsps2Failed ->
            state.copy(
                currentInvoice = null,
                lastOutcome = "Invoice request failed: ${event.reason}",
            )
        // KTD-5: the Event enum grows ahead of the shells (U5 added backup /
        // fence / sweep / recovery / restore variants fired by later units);
        // reducers ignore unrecognized variants defensively.
        else -> state
    }
