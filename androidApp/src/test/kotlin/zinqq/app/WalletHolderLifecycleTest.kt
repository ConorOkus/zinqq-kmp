package zinqq.app

import androidx.datastore.preferences.core.PreferenceDataStoreFactory
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.LifecycleOwner
import java.io.File
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertFalse
import kotlin.test.assertIs
import kotlin.test.assertTrue
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.delay
import kotlinx.coroutines.runBlocking
import uniffi.wallet_core.Event
import uniffi.wallet_core.Wallet
import uniffi.wallet_core.WalletException
import zinqq.app.theme.SettingsRepository

/**
 * Regression cover for the foreground node lifecycle's serialization (KTD-10),
 * the thing standing between a rapid background/foreground flip and two
 * `ChannelManager`s on one seed (see [WalletHolder]'s class doc).
 *
 * The Android twin of iOS's `EventLoopSingleConsumerTests`: a fake
 * [WalletNode] records the transitions the chain actually applied and counts
 * how many consumers sit inside the event loop at once — the core's
 * handle-then-ack queue allows exactly one. The tests drive rapid start/stop
 * sequences plus a restore straddled by a lifecycle flip and assert that
 * transitions apply in request order, the settled node state matches the last
 * request, no stop is dropped, and the loop is never double-consumed.
 */
class WalletHolderLifecycleTest {

    /** The observer callbacks never touch the owner. */
    private val owner = object : LifecycleOwner {
        override val lifecycle: Lifecycle get() = error("not used by DefaultLifecycleObserver")
    }

    /** Mutable stand-in for `ProcessLifecycleOwner`'s current state. */
    private class Foreground(@Volatile var value: Boolean = true)

    /**
     * A [WalletNode] with no native handle: it tracks whether the node is
     * running, logs every transition it actually applied, and — like iOS's
     * `FakeEventQueue` — holds each event-loop run open briefly so an
     * overlapping second consumer would be observable rather than invisible.
     */
    private class FakeWalletNode : WalletNode {
        override val wallet: Wallet? = null

        private val queue = Channel<Event>(Channel.UNLIMITED)
        private val transitions = mutableListOf<String>()
        private val restoreEntered = CountDownLatch(1)
        private val restoreRelease = CountDownLatch(1)
        private var concurrentLoops = 0

        /** Set before [restore] is reached to hold the sequence mid-flight. */
        @Volatile
        var blockRestore = false

        var running = false
            private set
        var startAttempts = 0
            private set
        var stopAttempts = 0
            private set

        /** Stops asked of an already-stopped node (a request that did nothing). */
        var swallowedStops = 0
            private set
        var restoreCalls = 0
            private set

        /** The core's `restore()` is stopped-only; this must never be set. */
        var restoreSawRunningNode = false
            private set
        var maxConcurrentLoops = 0
            private set

        /** Transitions that took effect, in the order the chain applied them. */
        val appliedTransitions: List<String> get() = synchronized(this) { transitions.toList() }

        override fun start() = synchronized(this) {
            startAttempts++
            if (running) throw WalletException.AlreadyRunning()
            running = true
            transitions += "start"
        }

        override fun stop() = synchronized(this) {
            stopAttempts++
            if (!running) {
                swallowedStops++
                throw WalletException.NotRunning()
            }
            running = false
            transitions += "stop"
            // A stop that transitioned pushes the terminal event that lets the
            // running loop drain and exit (KTD-8).
            queue.trySend(Event.NodeStopped)
            Unit
        }

        override fun restore(mnemonic: String) {
            synchronized(this) {
                restoreCalls++
                if (running) restoreSawRunningNode = true
                transitions += "restore"
            }
            restoreEntered.countDown()
            if (blockRestore) {
                assertTrue(restoreRelease.await(5, TimeUnit.SECONDS), "restore never released")
            }
        }

        override suspend fun runEventLoop(onEvent: suspend (Event) -> Unit) {
            synchronized(this) {
                concurrentLoops++
                maxConcurrentLoops = maxOf(maxConcurrentLoops, concurrentLoops)
            }
            try {
                // Hold this run inside the loop long enough for a would-be
                // second consumer to overlap (iOS's fake queue sleeps 20 ms
                // for the same reason).
                delay(20)
                while (true) {
                    val event = queue.receive()
                    onEvent(event)
                    if (event is Event.NodeStopped) return
                }
            } finally {
                synchronized(this) { concurrentLoops-- }
            }
        }

        fun push(event: Event) {
            queue.trySend(event)
        }

        fun awaitRestoreEntered() {
            assertTrue(restoreEntered.await(5, TimeUnit.SECONDS), "restore was never reached")
        }

        fun releaseRestore() {
            restoreRelease.countDown()
        }
    }

    private fun lifecycleTest(
        body: suspend (WalletHolder, FakeWalletNode, Foreground) -> Unit,
    ) {
        val dataStoreScope = CoroutineScope(SupervisorJob() + Dispatchers.IO)
        val dir = File.createTempFile("wallet-holder", null).apply { delete(); mkdirs() }
        val node = FakeWalletNode()
        val foreground = Foreground()
        try {
            val settings = SettingsRepository(
                PreferenceDataStoreFactory.create(scope = dataStoreScope) {
                    File(dir, "settings.preferences_pb")
                },
            )
            val holder = WalletHolder(
                settings = settings,
                isForegrounded = { foreground.value },
                nodeFactory = { node },
            )
            runBlocking {
                body(holder, node, foreground)
                // Terminate whatever loop run is parked on the queue so the
                // single-consumer counter has settled before the assertions
                // above are trusted, and no run outlives the test.
                node.push(Event.NodeStopped)
                holder.awaitEventLoopIdle()
            }
        } finally {
            dir.deleteRecursively()
        }
    }

    // --- rapid background/foreground flips ---

    @Test
    fun rapidFlipsApplyInOrderAndSettleOnTheFinalRequest() = lifecycleTest { holder, node, _ ->
        // ON_START/ON_STOP/ON_START/ON_STOP/ON_START with no waiting between:
        // every link is queued while the previous one is still running.
        holder.onStart(owner)
        holder.onStop(owner)
        holder.onStart(owner)
        holder.onStop(owner)
        holder.onStart(owner)
        holder.awaitLifecycleIdle()

        assertEquals(
            listOf("start", "stop", "start", "stop", "start"),
            node.appliedTransitions,
        )
        // The last request was a start, so the node is running — a stale stop
        // running after a fresh start is exactly the fund-safety bug.
        assertTrue(node.running)
        assertEquals(0, node.swallowedStops)
        assertEquals(1, node.maxConcurrentLoops)
    }

    @Test
    fun rapidFlipsEndingOnAStopLeaveTheNodeStopped() = lifecycleTest { holder, node, _ ->
        holder.onStart(owner)
        holder.onStop(owner)
        holder.onStart(owner)
        holder.onStop(owner)
        holder.awaitLifecycleIdle()

        assertEquals(listOf("start", "stop", "start", "stop"), node.appliedTransitions)
        assertFalse(node.running)
        assertEquals(0, node.swallowedStops)
    }

    @Test
    fun aFlippedEventLoopNeverHasTwoConcurrentConsumers() = lifecycleTest { holder, node, _ ->
        holder.onStart(owner)
        node.push(Event.SyncCompleted)
        holder.onStop(owner)
        holder.onStart(owner)
        node.push(Event.NodeStarted)
        holder.onStop(owner)
        holder.onStart(owner)
        holder.awaitLifecycleIdle()
        node.push(Event.NodeStopped)
        holder.awaitEventLoopIdle()

        assertEquals(1, node.maxConcurrentLoops)
    }

    // --- restore straddled by a lifecycle flip ---

    @Test
    fun anOnStopDuringRestoreIsNeverSwallowed() = lifecycleTest { holder, node, _ ->
        holder.onStart(owner)
        holder.awaitLifecycleIdle()
        assertTrue(node.running)

        node.blockRestore = true
        holder.startRestore(TEST_MNEMONIC)
        node.awaitRestoreEntered()

        // The process backgrounds while restore() is still blocking. The
        // foreground flag is deliberately left `true`: that is the stale read
        // the restore's own restart used to act on, and the queued stop is
        // what has to correct it.
        holder.onStop(owner)
        node.releaseRestore()
        holder.awaitLifecycleIdle()

        // restore's stop → restore → foreground-gated restart, and only then
        // the ON_STOP that arrived mid-restore.
        assertEquals(
            listOf("start", "stop", "restore", "start", "stop"),
            node.appliedTransitions,
        )
        assertFalse(node.running)
        assertFalse(node.restoreSawRunningNode)
        assertIs<RestoreUi.Succeeded>(holder.state.value.restore)
    }

    @Test
    fun aRestoreFinishingWhileBackgroundedLeavesNoHeadlessNode() =
        lifecycleTest { holder, node, foreground ->
            holder.onStart(owner)
            holder.awaitLifecycleIdle()

            node.blockRestore = true
            holder.startRestore(TEST_MNEMONIC)
            node.awaitRestoreEntered()
            // Backgrounded before the restart's foreground read.
            foreground.value = false
            node.releaseRestore()
            holder.awaitLifecycleIdle()

            assertEquals(listOf("start", "stop", "restore"), node.appliedTransitions)
            assertFalse(node.running)
        }

    @Test
    fun anOnStartDuringRestoreLandsAfterIt() = lifecycleTest { holder, node, _ ->
        node.blockRestore = true
        holder.startRestore(TEST_MNEMONIC)
        node.awaitRestoreEntered()

        // A foreground start arriving mid-restore must not race the core's
        // stopped-only restore(); it takes its turn after the whole sequence.
        holder.onStart(owner)
        node.releaseRestore()
        holder.awaitLifecycleIdle()

        // The node was already stopped, so restore's own stop did nothing; its
        // foreground-gated restart brought the node up, and the queued
        // ON_START is an AlreadyRunning no-op success.
        assertEquals(listOf("restore", "start"), node.appliedTransitions)
        assertEquals(2, node.startAttempts)
        assertTrue(node.running)
        assertFalse(node.restoreSawRunningNode)

        // ...and a following background stop still lands.
        holder.onStop(owner)
        holder.awaitLifecycleIdle()
        assertFalse(node.running)
    }

    @Test
    fun theEventLoopExitsOnNodeStoppedAndRestartsCleanly() = lifecycleTest { holder, node, _ ->
        holder.onStart(owner)
        holder.awaitLifecycleIdle()
        holder.onStop(owner)
        holder.awaitLifecycleIdle()
        holder.awaitEventLoopIdle()

        // A fresh foreground start schedules a new run over the same queue.
        holder.onStart(owner)
        holder.awaitLifecycleIdle()
        node.push(Event.SyncCompleted)
        node.push(Event.NodeStopped)
        holder.awaitEventLoopIdle()

        assertEquals(1, node.maxConcurrentLoops)
        assertEquals(listOf("start", "stop", "start"), node.appliedTransitions)
    }

    private companion object {
        /** The fake node does not validate; any 12 words stand in (U1/F3). */
        const val TEST_MNEMONIC =
            "abandon abandon abandon abandon abandon abandon " +
                "abandon abandon abandon abandon abandon about"
    }
}
