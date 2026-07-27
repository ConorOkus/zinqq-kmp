package zinqq.app

import uniffi.wallet_core.ActivityRow
import uniffi.wallet_core.Balances
import uniffi.wallet_core.CloseRecordView
import uniffi.wallet_core.Event
import uniffi.wallet_core.PendingSweepView
import uniffi.wallet_core.RecoveryStateView
import zinqq.app.theme.AppearanceMode

/** The invoice currently on screen, straight off [Event.InvoiceReady]. */
data class InvoiceUi(
    val bolt11: String,
    val expiryUnixSecs: ULong,
)

/**
 * The channel-close detail query result (U14): [record] is null when the
 * core has no record for [channelId] ("Close record not found"), while a
 * missing [CloseDetailUi] altogether means the query hasn't run yet.
 */
data class CloseDetailUi(
    val channelId: String,
    val record: CloseRecordView?,
)

/**
 * The Restore screen's live progress and terminal outcome (U17, F3). Held on
 * [UiState] because the holder owns the whole stop → restore → restart
 * sequence in its process scope: leaving the screen mid-restore must not
 * orphan a stopped node, and the screen re-attaches to whatever phase is
 * current. `null` = no restore this session.
 */
sealed interface RestoreUi {
    /** [step] is the PWA's exact progress copy from `RestoreProgress` events. */
    data class InProgress(val step: String) : RestoreUi
    data object Succeeded : RestoreUi
    data class Failed(val message: String) : RestoreUi
}

/** Immutable screen state; only [reduce] and wallet-data refreshes produce new values. */
data class UiState(
    val nodeRunning: Boolean = false,
    val balanceMsat: ULong = 0uL,
    val onchainSats: ULong = 0uL,
    val currentInvoice: InvoiceUi? = null,
    val lastOutcome: String? = null,
    val syncBanner: String? = null,
    /**
     * Another client took over this seed's VSS namespace (U13; KTD-3, plan
     * System-Wide Impact): set by [Event.Fenced] or a typed `Fenced` start
     * failure, never cleared by an event — un-fencing is user-owned (restore
     * or quit) and the core's durable flag survives restart.
     */
    val fenced: Boolean = false,
    /** Persisted appearance selection (U13, KTD-11); mirrored from DataStore. */
    val appearanceMode: AppearanceMode = AppearanceMode.DEFAULT,
    /** Persisted `balance-visible` toggle (R12); mirrored from DataStore. */
    val balanceVisible: Boolean = true,
    /** Last `balances()` snapshot; null until the first refresh (loading). */
    val balances: Balances? = null,
    /** Last `list_activity()` snapshot; null until the first refresh (loading). */
    val activity: List<ActivityRow>? = null,
    /** Force-close recovery state; null = no recovery in progress (R9). */
    val recoveryState: RecoveryStateView? = null,
    /**
     * Session-local hide of the sweep-confirmed success banner. The PWA's
     * dismiss durably clears the recovery state; the core exposes no clear
     * call yet, so dismissal lives here and resets whenever
     * [Event.RecoveryStateChanged] announces fresh state.
     */
    val recoveryBannerDismissed: Boolean = false,
    /** Outputs waiting to sweep; the banner gates on `lastAttemptFailed` (R8). */
    val pendingSweep: PendingSweepView? = null,
    /** The close-detail screen's current query result. */
    val closeDetail: CloseDetailUi? = null,
    /**
     * Fatal start failure — Home replaces its content with the PWA's
     * "Something went wrong" state (`Home.tsx:29-42`).
     */
    val startError: String? = null,
    /**
     * This node's pubkey for the Advanced screen's copy card (U17): queried
     * on every wallet-data refresh, kept cached across stops (`node_id()`
     * needs a running node) — the card simply doesn't render before the
     * first successful start, like the PWA's not-ready gate.
     */
    val nodeId: String? = null,
    /** The Restore flow's live phase (U17, F3); null = no restore running. */
    val restore: RestoreUi? = null,
)

/**
 * The events after which the wallet-data snapshots (balances, activity,
 * recovery state, pending sweep) must be re-queried: the spike's balance
 * triggers extended with the sweep/recovery change events (U14; the PWA's
 * hooks re-read on the equivalent change notifications).
 */
fun shouldRefreshWalletData(event: Event): Boolean = when (event) {
    is Event.PaymentReceived,
    is Event.PaymentSuccessful,
    is Event.ChannelReady,
    is Event.SweepStateChanged,
    is Event.RecoveryStateChanged,
    -> true
    else -> false
}

/**
 * Pure event-to-state reduction, unit-testable without Android or a wallet
 * instance. No Lightning logic lives here (R14): event fields pass through to
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
        // The core fenced itself (KTD-3): the shell blocks every destination
        // behind the fenced screen until the user restores or quits (U13).
        is Event.Fenced -> state.copy(fenced = true)
        // Fresh recovery state invalidates a session-local banner dismissal
        // (the holder re-queries the state itself; see shouldRefreshWalletData).
        is Event.RecoveryStateChanged -> state.copy(recoveryBannerDismissed = false)
        // U17/F3: the core's restore emits the PWA's exact step copy; it only
        // advances an in-progress restore — a stray late event can neither
        // start one nor overwrite a terminal outcome.
        is Event.RestoreProgress ->
            if (state.restore is RestoreUi.InProgress) {
                state.copy(restore = RestoreUi.InProgress(event.step))
            } else {
                state
            }
        // KTD-5: the Event enum grows ahead of the shells (U5 added backup /
        // sweep / recovery / restore variants fired by later units); reducers
        // ignore unrecognized variants defensively.
        else -> state
    }
