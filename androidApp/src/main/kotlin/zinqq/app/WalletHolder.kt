package zinqq.app

import android.content.Context
import androidx.lifecycle.DefaultLifecycleObserver
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.LifecycleOwner
import androidx.lifecycle.ProcessLifecycleOwner
import java.io.File
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.channels.BufferOverflow
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.MutableSharedFlow
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asSharedFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.update
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import uniffi.wallet_core.ChannelView
import uniffi.wallet_core.ClassifiedView
import uniffi.wallet_core.CloseEstimate
import uniffi.wallet_core.Event
import uniffi.wallet_core.FeeEstimate
import uniffi.wallet_core.JitInvoice
import uniffi.wallet_core.JitQuote
import uniffi.wallet_core.LnurlPayView
import uniffi.wallet_core.MaxSendEstimate
import uniffi.wallet_core.OpenFeeEstimate
import uniffi.wallet_core.PeerView
import uniffi.wallet_core.ReceiveBundle
import uniffi.wallet_core.ResolvedView
import uniffi.wallet_core.Wallet
import uniffi.wallet_core.WalletException
import uniffi.wallet_core.deriveDebugInfo
import zinqq.app.screens.receive.ReceivePort
import zinqq.app.screens.receive.usableInboundMsat
import zinqq.app.screens.send.SendPort
import zinqq.app.screens.settings.RESTORE_INITIAL_STEP
import zinqq.app.screens.settings.SettingsPort
import zinqq.app.screens.settings.restoreErrorMessage
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
) : DefaultLifecycleObserver, SendPort, ReceivePort, SettingsPort {
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

    // U15: live rebroadcast of core events so the send flow can await its
    // payment outcome (F1). No replay — a subscriber must exist before the
    // dispatch it cares about; DROP_OLDEST so the event loop never blocks on
    // a slow collector (the durable queue in the core is the real record).
    private val _events = MutableSharedFlow<Event>(
        extraBufferCapacity = 64,
        onBufferOverflow = BufferOverflow.DROP_OLDEST,
    )
    override val walletEvents: Flow<Event> = _events.asSharedFlow()

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
            // Cached across stops (U17): node_id() needs a running node, but
            // the pubkey is stable for the wallet's lifetime, so fetch it only
            // until it caches ([startRestore] clears it for the new wallet).
            val freshNodeId = if (_state.value.nodeId == null) {
                try {
                    wallet.nodeId()
                } catch (_: Exception) {
                    null
                }
            } else {
                null
            }
            _state.update { state ->
                state.copy(
                    balances = balances ?: state.balances,
                    activity = activity ?: state.activity,
                    recoveryState = recovery,
                    pendingSweep = sweep,
                    nodeId = freshNodeId ?: state.nodeId,
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

    // ------------------------------------------------------------------
    // SendPort (U15, R14): thin passthroughs to the core's send FFI. All
    // blocking calls hop to IO; `resolveInput`/`fetchLnurlInvoice` are
    // already suspend bindings polled by the foreign executor.
    // ------------------------------------------------------------------

    override suspend fun classify(input: String): ClassifiedView =
        withContext(Dispatchers.IO) { wallet.classifyInput(input) }

    override suspend fun resolve(input: String): ResolvedView = wallet.resolveInput(input)

    override suspend fun fetchLnurlInvoice(
        lnurl: LnurlPayView,
        amountMsat: ULong,
    ): ClassifiedView = wallet.fetchLnurlInvoice(lnurl, amountMsat)

    override suspend fun sendBolt11(bolt11: String, amountMsat: ULong?) =
        withContext(Dispatchers.IO) { wallet.sendBolt11(bolt11, amountMsat) }

    override suspend fun payOffer(offer: String, amountMsat: ULong?) =
        withContext(Dispatchers.IO) { wallet.payOffer(offer, amountMsat, payerNote = null) }

    override suspend fun estimateOnchainFee(address: String, amountSats: ULong): FeeEstimate =
        withContext(Dispatchers.IO) { wallet.estimateOnchainFee(address, amountSats) }

    override suspend fun estimateMaxSendable(address: String): MaxSendEstimate =
        withContext(Dispatchers.IO) { wallet.estimateMaxSendable(address) }

    override suspend fun sendOnchain(
        address: String,
        amountSats: ULong,
        expectedAmountSats: ULong,
        expectedFeeSats: ULong,
    ): String = withContext(Dispatchers.IO) {
        wallet.sendOnchain(address, amountSats, expectedAmountSats, expectedFeeSats)
    }

    override suspend fun sendOnchainMax(
        address: String,
        expectedAmountSats: ULong,
        expectedFeeSats: ULong,
    ): String = withContext(Dispatchers.IO) {
        wallet.sendOnchainMax(address, expectedAmountSats, expectedFeeSats)
    }

    override fun lightningCapacityMsat(): ULong =
        _state.value.balances?.lightningMsat ?: 0uL

    // The PWA's onchainBalance is confirmed + trusted pending
    // (`Send.tsx:164-165`) = total − untrusted pending, both core-computed.
    override fun onchainBalanceSats(): ULong =
        _state.value.balances?.let { it.onchainTotalSats - it.onchainUntrustedPendingSats }
            ?: 0uL

    // ------------------------------------------------------------------
    // ReceivePort (U16, R14): thin passthroughs to the core's receive FFI.
    // The capacity decision, live floor, quote/buy protocol, and expiry
    // clamp all live in Rust; blocking calls hop to IO.
    // ------------------------------------------------------------------

    override suspend fun receiveBundle(amountMsat: ULong?): ReceiveBundle =
        withContext(Dispatchers.IO) { wallet.receiveBundle(amountMsat) }

    override suspend fun jitQuote(amountMsat: ULong): JitQuote =
        withContext(Dispatchers.IO) { wallet.jitQuote(amountMsat) }

    override suspend fun jitAccept(quoteToken: ULong, amountMsat: ULong): JitInvoice =
        withContext(Dispatchers.IO) { wallet.jitAccept(quoteToken, amountMsat) }

    override suspend fun minReceiveSats(refresh: Boolean): ULong =
        withContext(Dispatchers.IO) { wallet.minReceiveSats(refresh) }

    override suspend fun usableInboundMsat(): ULong =
        withContext(Dispatchers.IO) { usableInboundMsat(wallet.listChannels()) }

    override suspend fun buildUnifiedUri(
        address: String,
        amountSats: ULong?,
        invoice: String?,
    ): String = withContext(Dispatchers.IO) {
        uniffi.wallet_core.buildBip321Uri(address, amountSats, invoice)
    }

    // ------------------------------------------------------------------
    // SettingsPort (U17, R14): thin passthroughs to the core's mnemonic and
    // channels/peers FFI. Bounds, guards, close estimates, and the connect
    // protocol all live in Rust; blocking calls hop to IO.
    // ------------------------------------------------------------------

    override suspend fun revealMnemonic(): String =
        withContext(Dispatchers.IO) { wallet.revealMnemonic() }

    override suspend fun validateMnemonic(mnemonic: String): Boolean =
        withContext(Dispatchers.IO) {
            // derive_debug_info is the exported BIP39 check (U1): it fails
            // typed (InvalidMnemonic) on anything but valid 12 English words.
            try {
                deriveDebugInfo(mnemonic)
                true
            } catch (e: kotlin.coroutines.cancellation.CancellationException) {
                throw e
            } catch (_: Exception) {
                false
            }
        }

    override suspend fun listPeers(): List<PeerView> =
        withContext(Dispatchers.IO) { wallet.listPeers() }

    override suspend fun listChannels(): List<ChannelView> =
        withContext(Dispatchers.IO) { wallet.listChannels() }

    override suspend fun forgetPeer(pubkey: String) =
        withContext(Dispatchers.IO) { wallet.forgetPeer(pubkey) }

    override suspend fun openChannel(peerAddress: String, amountSats: ULong): String =
        withContext(Dispatchers.IO) { wallet.openChannel(peerAddress, amountSats) }

    override suspend fun estimateOpenFee(): OpenFeeEstimate =
        withContext(Dispatchers.IO) { wallet.estimateOpenFee() }

    override suspend fun estimateClose(channelId: String): CloseEstimate =
        withContext(Dispatchers.IO) { wallet.estimateClose(channelId) }

    override suspend fun closeChannel(channelId: String, force: Boolean) =
        withContext(Dispatchers.IO) { wallet.closeChannel(channelId, force) }

    private fun startNode() {
        // A running restore owns the node lifecycle (stop → restore →
        // restart); a foreground start racing it would make the core's
        // stopped-only restore() fail with AlreadyRunning.
        if (_state.value.restore is RestoreUi.InProgress) return
        scope.launch {
            if (_state.value.restore is RestoreUi.InProgress) return@launch
            if (!startCore()) return@launch
            ensureEventLoop()
            refreshWalletData()
        }
    }

    /**
     * Blocking `start()` with the typed outcomes mapped into state; `true`
     * when the node is up (an `AlreadyRunning` start is a no-op success —
     * e.g. a foreground flip while the just-stopped loop still drains).
     */
    private suspend fun startCore(): Boolean = withContext(Dispatchers.IO) {
        try {
            wallet.start()
            _state.update { it.copy(startError = null) }
            true
        } catch (e: WalletException.AlreadyRunning) {
            _state.update { it.copy(startError = null) }
            true
        } catch (e: WalletException.Fenced) {
            // The durable fence survives restart (KTD-3): a fenced wallet
            // refuses to start, and the shell must block every screen even
            // though no Event.Fenced will arrive on this run (U13).
            _state.update { it.copy(fenced = true) }
            false
        } catch (e: Exception) {
            // Home replaces its content with the PWA's error state
            // ("Something went wrong", Home.tsx:29-42).
            _state.update {
                it.copy(
                    lastOutcome = "Node start failed: ${e.message}",
                    startError = e.message ?: "Node start failed",
                )
            }
            false
        }
    }

    /**
     * Starts the shared event loop unless one is already draining. Exactly
     * one loop may consume the core's handle-then-ack queue at a time;
     * synchronized because foreground starts (main) and the restore sequence
     * (IO) both call this.
     */
    @Synchronized
    private fun ensureEventLoop() {
        if (loopJob?.isActive == true) return
        loopJob = scope.launch {
            // Handle-then-ack inside WalletCore; returns after acking the
            // terminal NodeStopped pushed by stop() (KTD-8).
            WalletCore.runEventLoop(wallet) { event ->
                _state.update { reduce(it, event) }
                // Rebroadcast AFTER the reduce so subscribers (the send
                // flow's outcome await, U15) observe state and event in order.
                _events.emit(event)
                if (shouldRefreshWalletData(event)) refreshWalletData()
            }
        }
    }

    /**
     * F3 (U17): replace the current wallet from 12 validated words. The
     * core's `restore()` is valid only from a stopped node, so the holder
     * owns the whole sequence — stop → hand the queue back to a fresh loop
     * (progress arrives as [Event.RestoreProgress], reduced into
     * [UiState.restore]) → blocking restore → restart → refresh. Runs in the
     * process scope: leaving the screen mid-restore cannot orphan a stopped
     * node, and the screen re-attaches to whatever phase is current.
     */
    fun startRestore(mnemonic: String) {
        if (_state.value.restore is RestoreUi.InProgress) return
        _state.update { it.copy(restore = RestoreUi.InProgress(RESTORE_INITIAL_STEP)) }
        scope.launch {
            // Any stop that actually transitioned pushes the terminal
            // NodeStopped, which lets the current loop drain and exit —
            // join it before starting the drain loop so exactly one consumer
            // ever holds the queue. A NotRunning stop pushes nothing (the
            // loop, if any, is already parked or gone).
            val pushedStop = try {
                wallet.stop()
                true
            } catch (_: WalletException.NotRunning) {
                false
            } catch (_: Exception) {
                // Stop-with-failed-final-persist still transitioned and
                // still pushed NodeStopped (see api.rs stop()).
                true
            }
            if (pushedStop) loopJob?.join()
            // Drain RestoreProgress events live while restore() blocks.
            ensureEventLoop()
            try {
                wallet.restore(mnemonic)
            } catch (e: Exception) {
                // The typed failures leave local state untouched — restart
                // the existing wallet so the app stays usable behind the
                // error screen (a still-fenced wallet re-fences here).
                restartAfterRestore()
                _state.update { it.copy(restore = RestoreUi.Failed(restoreErrorMessage(e))) }
                return@launch
            }
            // The restored wallet replaced the old one — any fence fell with
            // it (a start failure still surfaces through startError on Home),
            // and its node id must be re-fetched.
            _state.update { it.copy(fenced = false, nodeId = null) }
            restartAfterRestore()
            _state.update { it.copy(restore = RestoreUi.Succeeded) }
        }
    }

    /**
     * Restore's exit restart, foreground-gated (KTD-10): a restore that
     * finishes while the app is backgrounded must not leave a headless node
     * running past its missed ON_STOP — the next foreground start covers it.
     */
    private suspend fun restartAfterRestore() {
        val foregrounded = ProcessLifecycleOwner.get()
            .lifecycle.currentState.isAtLeast(Lifecycle.State.STARTED)
        if (!foregrounded) return
        startCore()
        refreshWalletData()
    }

    /** The Restore screen's Try-Again/exit ack; a running restore stays owned. */
    fun clearRestore() {
        _state.update {
            if (it.restore is RestoreUi.InProgress) it else it.copy(restore = null)
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
