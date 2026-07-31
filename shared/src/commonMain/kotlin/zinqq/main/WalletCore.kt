package zinqq.main

// The uniffi.wallet_core package is generated at build time by the Gobley
// uniffi plugin (library mode) from the wallet-core cdylib; UniFFI lower-camels
// the exported Rust fn names (core_version -> coreVersion) and maps the
// exported Wallet object / Event enum / WalletConfig record to Kotlin types.
import kotlinx.coroutines.CoroutineDispatcher
import kotlinx.coroutines.withContext
import uniffi.wallet_core.Event
import uniffi.wallet_core.Wallet
import uniffi.wallet_core.WalletConfig
import uniffi.wallet_core.WalletNetwork
import uniffi.wallet_core.coreVersion as coreVersionBinding
import uniffi.wallet_core.pingAsync as pingAsyncBinding

/**
 * Thin common wrapper over the generated wallet-core bindings. Platform shells
 * talk to this object, never to the generated package directly.
 */
object WalletCore {
    /** Crate version plus a secp256k1-derived pubkey, computed in Rust. */
    fun coreVersion(): String = coreVersionBinding()

    /** Round-trips the core-owned tokio runtime through a suspend binding. */
    suspend fun pingAsync(): String = pingAsyncBinding()

    /**
     * Creates a stopped wallet over an app-private storage directory,
     * reloading any persisted (unacked) events. No seed input (AE2). The URL
     * overrides exist for tests and fallback.
     *
     * [network] is chosen at build time by the shell — debug builds may target
     * Mutinynet, Release/TestFlight is always mainnet. `null` means mainnet,
     * so anything that says nothing gets the production network. Note that
     * Kotlin default arguments do not export to Swift, so iOS passes this
     * explicitly.
     *
     * The core isolates each network's storage directory and VSS store, so two
     * networks over the same [storageDir] never share state.
     */
    fun create(
        storageDir: String,
        esploraUrl: String? = null,
        rgsUrl: String? = null,
        network: WalletNetwork? = null,
    ): Wallet =
        Wallet(
            WalletConfig(
                storageDir = storageDir,
                esploraUrl = esploraUrl,
                rgsUrl = rgsUrl,
                network = network,
            ),
        )

    /**
     * Handle-then-ack event loop (KTD-8). Each event is passed to [onEvent]
     * BEFORE it is acked, so a crash between handling and acking redelivers
     * the same event on the next loop or restart — handlers must be
     * idempotent. Runs on [ioDispatcher] (`Dispatchers.IO` is unavailable in
     * commonMain on Kotlin/Native) and returns after handling and acking the
     * terminal [Event.NodeStopped] (which `stop()` pushes to complete a
     * pending `nextEvent`); restart the loop after the next `start()` on
     * foreground (KTD-10).
     */
    suspend fun runEventLoop(
        wallet: Wallet,
        onEvent: suspend (Event) -> Unit,
    ) {
        withContext(ioDispatcher) {
            while (true) {
                val event = wallet.nextEvent()
                onEvent(event)
                wallet.eventHandled()
                if (event is Event.NodeStopped) return@withContext
            }
        }
    }
}

/**
 * Dispatcher for the blocking FFI calls the event loop makes. `Dispatchers.IO`
 * is JVM/Android-only from commonMain, so each platform supplies its own.
 */
internal expect val ioDispatcher: CoroutineDispatcher
