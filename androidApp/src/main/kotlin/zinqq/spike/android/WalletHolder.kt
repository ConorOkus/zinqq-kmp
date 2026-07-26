package zinqq.spike.android

import android.content.Context
import androidx.lifecycle.DefaultLifecycleObserver
import androidx.lifecycle.LifecycleOwner
import androidx.lifecycle.ProcessLifecycleOwner
import java.io.File
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.update
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import uniffi.wallet_core.Event
import uniffi.wallet_core.Wallet
import zinqq.spike.WalletCore

/**
 * Process-scoped owner of the wallet, its event loop, and the foreground-only
 * node lifecycle (KTD-10).
 *
 * The node's lifetime is a property of the *process*, not of any activity: it
 * holds a tokio runtime, TCP connections to the LSP, and an exclusive lock on
 * the storage directory. Scoping it to a ViewModel made those outlive their
 * owner — an activity destroyed by a back press cleared the ViewModel, which
 * cancelled the scope its own stop call needed and detached the lifecycle
 * observer before `ProcessLifecycleOwner`'s delayed `onStop` could fire. The
 * node kept running headless in the cached process, and relaunching built a
 * second wallet over the same directory: two `ChannelManager`s on one seed with
 * last-writer-wins persistence.
 *
 * Created once from [SpikeApplication], so activity recreation (rotation, back
 * press and relaunch) reuses the running node instead of racing a second one.
 * The Rust core independently refuses a second instance over the same storage
 * directory, so this class is the ergonomic half of that guarantee, not its
 * only line of defence.
 */
class WalletHolder(context: Context) : DefaultLifecycleObserver {
    // App-private filesDir (NOT cache, which the OS may purge): holds the seed,
    // channel monitors, and the storage lock, and is the directory
    // data_extraction_rules.xml excludes from backup and device transfer (R6).
    private val storageDir = File(context.filesDir, "wallet").absolutePath

    // Outlives every activity, unlike viewModelScope. SupervisorJob so one
    // failed intent cannot tear down the event loop.
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)

    private val wallet: Wallet by lazy { WalletCore.create(storageDir) }

    private val _state = MutableStateFlow(UiState())
    val state: StateFlow<UiState> = _state.asStateFlow()

    private var loopJob: Job? = null

    fun observeProcessLifecycle() {
        ProcessLifecycleOwner.get().lifecycle.addObserver(this)
    }

    override fun onStart(owner: LifecycleOwner) = startNode()

    override fun onStop(owner: LifecycleOwner) = stopNode()

    fun refreshBalances() {
        scope.launch {
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
        scope.launch {
            try {
                wallet.receiveJit(amountSats * 1_000uL)
            } catch (e: Exception) {
                // Generated WalletException: the Lsps2Failed event carries the
                // same reason, but a typed failure must surface even if it beat
                // the event loop.
                _state.update { it.copy(lastOutcome = "Invoice request failed: ${e.message}") }
            }
        }
    }

    fun sendPayment(bolt11: String) {
        scope.launch {
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
        loopJob = scope.launch {
            try {
                wallet.start()
            } catch (e: Exception) {
                _state.update { it.copy(lastOutcome = "Node start failed: ${e.message}") }
                return@launch
            }
            refreshBalances()
            // Handle-then-ack inside WalletCore; returns after acking the
            // terminal NodeStopped pushed by stop() (KTD-8).
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
        // Runs on the process-scoped scope, so it survives the activity teardown
        // that races this call on a back-press exit.
        scope.launch {
            try {
                wallet.stop()
            } catch (_: Exception) {
                // NotRunning: nothing to stop.
            }
        }
    }

    /**
     * Blocking best-effort stop for `Application.onTerminate`-style paths and
     * tests. Normal shutdown goes through [onStop]; this exists so a caller that
     * must not return until the node has released its storage lock can wait.
     */
    fun stopBlocking() {
        try {
            wallet.stop()
        } catch (_: Exception) {
            // NotRunning: nothing to stop.
        }
    }
}
