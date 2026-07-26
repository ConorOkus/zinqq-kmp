package zinqq.spike

import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.async
import kotlinx.coroutines.launch
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.withContext
import kotlinx.coroutines.withTimeout
import uniffi.wallet_core.Event
import kotlin.io.path.createTempDirectory
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertTrue

class BindingsSmokeTest {
    @Test
    fun coreVersionCrossesTheFfiBoundary() {
        val version = WalletCore.coreVersion()
        assertTrue(
            version.startsWith("wallet-core 0.1.0"),
            "unexpected version string from Rust: $version",
        )
    }

    @Test
    fun pingAsyncCompletesViaSuspendBinding() = runBlocking {
        assertEquals("pong", WalletCore.pingAsync())
    }

    /**
     * FFI threading proof for the U3 surface (KTD-8): start the node offline
     * (unreachable Esplora on a closed local port -> degraded start), pull
     * NodeStarted and SyncFailed through the suspend nextEvent binding in
     * handle-then-ack order, then stop() while a nextEvent await is
     * outstanding and observe it complete with the terminal NodeStopped
     * rather than hanging.
     */
    @Test
    fun nodeLifecycleEventsFlowAcrossTheFfiBoundary() = runBlocking {
        val storageDir = createTempDirectory("wallet-core-smoke").toString()
        val wallet =
            WalletCore.create(
                storageDir = storageDir,
                esploraUrl = "http://127.0.0.1:1",
                rgsUrl = "http://127.0.0.1:1/snapshot",
            )

        withContext(Dispatchers.IO) { wallet.start() }

        val first = withTimeout(30_000) { wallet.nextEvent() }
        assertTrue(first is Event.NodeStarted, "expected NodeStarted first, got $first")
        wallet.eventHandled()

        val second = withTimeout(30_000) { wallet.nextEvent() }
        assertTrue(
            second is Event.SyncFailed,
            "an offline degraded start must queue SyncFailed after NodeStarted, got $second",
        )
        wallet.eventHandled()

        // Lifecycle edge: the queue is now drained, so this await is pending
        // until stop() pushes the terminal event (runtime-independent wake).
        val pending = async(Dispatchers.IO) { wallet.nextEvent() }
        launch(Dispatchers.IO) { wallet.stop() }

        val terminal = withTimeout(30_000) { pending.await() }
        assertTrue(
            terminal is Event.NodeStopped,
            "stop() must complete a pending nextEvent with NodeStopped, got $terminal",
        )
        wallet.eventHandled()
    }
}
