package zinqq.app

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
import uniffi.wallet_core.Wallet
import uniffi.wallet_core.WalletException
import zinqq.app.theme.AppearanceMode
import zinqq.app.theme.SettingsRepository
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
 * Created once from [ZinqqApplication], so activity recreation (rotation, back
 * press and relaunch) reuses the running node instead of racing a second one.
 * The Rust core independently refuses a second instance over the same storage
 * directory, so this class is the ergonomic half of that guarantee, not its
 * only line of defence.
 *
 * This is also the only place uniffi types are touched (R14): screens read
 * [state] and call intent methods; classification, fee math, and protocol
 * work stay in the Rust core.
 */
class WalletHolder(
    context: Context,
    private val settings: SettingsRepository,
) : DefaultLifecycleObserver {
    // App-private filesDir (NOT cache, which the OS may purge): holds the seed,
    // channel monitors, and the storage lock, and is the directory
    // data_extraction_rules.xml excludes from backup and device transfer (R6).
    private val storageDir = File(context.filesDir, "wallet").absolutePath

    // Outlives every activity, unlike viewModelScope. SupervisorJob so one
    // failed intent cannot tear down the event loop.
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)

    private val wallet: Wallet by lazy { WalletCore.create(storageDir) }

    // The synchronous read is deliberate (KTD-11): the persisted appearance
    // mode must be in the very first emitted state so no frame renders in the
    // wrong theme. It runs once, at process start, against a tiny local file.
    private val _state = MutableStateFlow(
        UiState(appearanceMode = settings.appearanceModeBlocking()),
    )
    val state: StateFlow<UiState> = _state.asStateFlow()

    private var loopJob: Job? = null

    init {
        // Keep the state's appearance mode mirroring the persisted selection.
        scope.launch {
            settings.appearanceMode.collect { mode ->
                _state.update { it.copy(appearanceMode = mode) }
            }
        }
        // Same mirror for the persisted balance-visibility toggle (R12).
        scope.launch {
            settings.balanceVisible.collect { visible ->
                _state.update { it.copy(balanceVisible = visible) }
            }
        }
    }

    fun observeProcessLifecycle() {
        ProcessLifecycleOwner.get().lifecycle.addObserver(this)
    }

    override fun onStart(owner: LifecycleOwner) = startNode()

    override fun onStop(owner: LifecycleOwner) = stopNode()

    /** Persist a new appearance mode; [state] updates through the mirror above. */
    fun setAppearanceMode(mode: AppearanceMode) {
        scope.launch { settings.setAppearanceMode(mode) }
    }

    /** Persist a new balance-visibility choice; [state] updates via the mirror. */
    fun setBalanceVisible(visible: Boolean) {
        scope.launch { settings.setBalanceVisible(visible) }
    }

    /**
     * Dismiss the sweep-confirmed success banner: durable via the core
     * (`dismiss_recovery`, a no-op unless SweepConfirmed) plus the session
     * flag so the UI hides immediately (see UiState).
     */
    fun dismissRecoveryBanner() {
        _state.update { it.copy(recoveryBannerDismissed = true) }
        scope.launch {
            runCatching { wallet?.dismissRecovery() }
            refreshWalletData()
        }
    }

    /**
     * Re-query every wallet-data snapshot the screens render (U14): balances,
     * the unified activity feed, recovery state, pending sweep, and the open
     * close detail, if any. Home's refresh icon calls this directly; the
     * event loop calls it on [shouldRefreshWalletData] events. All derivation
     * happens in pure presentation functions (R14) — this only snapshots.
     */
    fun refreshWalletData() {
        scope.launch {
            // Balances/activity need a running node; keep the previous
            // snapshots when the query fails (e.g. refresh while stopped).
            val balances = try {
                wallet.balances()
            } catch (_: Exception) {
                null
            }
            val activity = try {
                wallet.listActivity()
            } catch (_: Exception) {
                null
            }
            // Local-first stores: readable even while stopped, and null is a
            // real answer (no recovery / nothing pending), not a failure.
            val recovery = wallet.recoveryState()
            val sweep = wallet.pendingSweep()
            _state.update { state ->
                state.copy(
                    balances = balances ?: state.balances,
                    balanceMsat = balances?.lightningMsat ?: state.balanceMsat,
                    onchainSats = balances?.onchainTotalSats ?: state.onchainSats,
                    activity = activity ?: state.activity,
                    recoveryState = recovery,
                    pendingSweep = sweep,
                    closeDetail = state.closeDetail?.let {
                        it.copy(record = wallet.closeDetail(it.channelId))
                    },
                )
            }
        }
    }

    /**
     * Load (or re-load) the close-detail screen's record. The screen renders
     * [UiState.closeDetail]; refreshes keep it live while the close resolves.
     */
    fun loadCloseDetail(channelId: String) {
        scope.launch {
            val record = wallet.closeDetail(channelId)
            _state.update { it.copy(closeDetail = CloseDetailUi(channelId, record)) }
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
            } catch (e: WalletException.Fenced) {
                // The durable fence survives restart (KTD-3): a fenced wallet
                // refuses to start, and the shell must block every screen even
                // though no Event.Fenced will arrive on this run (U13).
                _state.update { it.copy(fenced = true) }
                return@launch
            } catch (e: Exception) {
                // Home replaces its content with the PWA's error state
                // ("Something went wrong", Home.tsx:29-42).
                _state.update {
                    it.copy(
                        lastOutcome = "Node start failed: ${e.message}",
                        startError = e.message ?: "Node start failed",
                    )
                }
                return@launch
            }
            _state.update { it.copy(startError = null) }
            refreshWalletData()
            // Handle-then-ack inside WalletCore; returns after acking the
            // terminal NodeStopped pushed by stop() (KTD-8).
            WalletCore.runEventLoop(wallet) { event ->
                _state.update { reduce(it, event) }
                if (shouldRefreshWalletData(event)) refreshWalletData()
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
