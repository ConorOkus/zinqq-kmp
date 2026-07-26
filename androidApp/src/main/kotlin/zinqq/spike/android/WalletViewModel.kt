package zinqq.spike.android

import android.app.Application
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.DefaultLifecycleObserver
import androidx.lifecycle.LifecycleOwner
import androidx.lifecycle.ProcessLifecycleOwner
import androidx.lifecycle.viewModelScope
import java.io.File
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.update
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import uniffi.wallet_core.Event
import uniffi.wallet_core.Wallet
import zinqq.spike.WalletCore

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
    }

/**
 * Owns the wallet and the handle-then-ack event loop. Foreground-only node
 * lifecycle (KTD-10) is driven by [ProcessLifecycleOwner] rather than activity
 * callbacks so configuration changes and activity recreation don't bounce the
 * node: the process-level onStart/onStop pair fires exactly on app
 * foreground/background.
 */
class WalletViewModel(application: Application) :
    AndroidViewModel(application), DefaultLifecycleObserver {
    // App-private filesDir (NOT cache, which the OS may purge): holds the seed
    // and channel monitors, and is the directory data_extraction_rules.xml
    // excludes from backup and device transfer (R6).
    private val wallet: Wallet by lazy {
        WalletCore.create(File(application.filesDir, "wallet").absolutePath)
    }

    private val _state = MutableStateFlow(UiState())
    val state: StateFlow<UiState> = _state.asStateFlow()

    private var loopJob: Job? = null

    init {
        ProcessLifecycleOwner.get().lifecycle.addObserver(this)
    }

    override fun onStart(owner: LifecycleOwner) = startNode()

    override fun onStop(owner: LifecycleOwner) = stopNode()

    override fun onCleared() {
        ProcessLifecycleOwner.get().lifecycle.removeObserver(this)
    }

    fun refreshBalances() {
        viewModelScope.launch(Dispatchers.IO) {
            // Balances need a running node; a stopped-node refresh is a no-op.
            val balances = try {
                wallet.balances()
            } catch (_: Exception) {
                return@launch
            }
            _state.update {
                it.copy(balanceMsat = balances.lightningMsat, onchainSats = balances.onchainSats)
            }
        }
    }

    fun requestInvoice(amountSats: ULong) {
        viewModelScope.launch(Dispatchers.IO) {
            try {
                wallet.receiveJit(amountSats * 1_000uL)
            } catch (e: Exception) {
                // Generated WalletException: the Lsps2Failed event carries the
                // same reason, but a typed failure must surface even if it
                // beat the event loop.
                _state.update { it.copy(lastOutcome = "Invoice request failed: ${e.message}") }
            }
        }
    }

    fun sendPayment(bolt11: String) {
        viewModelScope.launch(Dispatchers.IO) {
            try {
                wallet.send(bolt11.trim())
            } catch (e: Exception) {
                _state.update { it.copy(lastOutcome = "Send failed: ${e.message}") }
            }
        }
    }

    private fun startNode() {
        // Still active while a just-stopped loop drains its terminal
        // NodeStopped; an instant background/foreground flip then skips this
        // start and the node stays stopped until the next foreground.
        // Acceptable for the spike.
        if (loopJob?.isActive == true) return
        loopJob = viewModelScope.launch {
            try {
                withContext(Dispatchers.IO) { wallet.start() }
            } catch (e: Exception) {
                _state.update { it.copy(lastOutcome = "Node start failed: ${e.message}") }
                return@launch
            }
            refreshBalances()
            // Handle-then-ack on Dispatchers.IO inside WalletCore; returns
            // after acking the terminal NodeStopped pushed by stop() (KTD-8).
            WalletCore.runEventLoop(wallet) { event ->
                _state.update { reduce(it, event) }
                when (event) {
                    is Event.PaymentReceived,
                    is Event.PaymentSuccessful,
                    is Event.ChannelReady,
                    -> refreshBalances()
                    else -> Unit
                }
            }
        }
    }

    private fun stopNode() {
        viewModelScope.launch(Dispatchers.IO) {
            try {
                wallet.stop()
            } catch (_: Exception) {
                // NotRunning: nothing to stop.
            }
        }
    }
}
