package zinqq.app

import uniffi.wallet_core.Event
import uniffi.wallet_core.Wallet
import uniffi.wallet_core.WalletException
import zinqq.spike.WalletCore

/**
 * The slice of the wallet the foreground node lifecycle drives (KTD-10):
 * start, stop, restore, and the handle-then-ack event loop.
 *
 * Extracted so [WalletHolder]'s lifecycle serialization — the thing standing
 * between a rapid background/foreground flip and two `ChannelManager`s on one
 * seed — can be exercised without an Android `Context`,
 * `ProcessLifecycleOwner`, or the uniffi native library. The iOS twin splits
 * `WalletEventSource` out of its event loop for the same reason.
 *
 * Everything else the shell does with the core stays on the per-screen Ports
 * (`SendPort`/`ReceivePort`/`SettingsPort`), which tests already fake.
 */
interface WalletNode {
    /** `Wallet.start()`; throws the typed [WalletException]s the shell maps. */
    fun start()

    /**
     * `Wallet.stop()`. A stop that actually transitioned pushes the terminal
     * [Event.NodeStopped] that lets a running event loop drain and exit;
     * a stop of an already-stopped node throws [WalletException.NotRunning]
     * and pushes nothing.
     */
    fun stop()

    /** `Wallet.restore()`; valid only from a stopped node. */
    fun restore(mnemonic: String)

    /**
     * Drains the core's persisted queue into [onEvent] handle-then-ack (KTD-8)
     * and returns after acking the terminal [Event.NodeStopped]. Exactly one
     * call may be in flight at a time — that is the queue's contract.
     */
    suspend fun runEventLoop(onEvent: suspend (Event) -> Unit)

    /**
     * The full FFI handle behind this node: the per-screen Port passthroughs
     * and the wallet-data snapshots go straight to it. Non-null in production,
     * where it is the same [Wallet] the calls above drive; `null` for a
     * lifecycle-only fake, whose tests never leave the lifecycle path.
     */
    val wallet: Wallet?
}

/**
 * The production [WalletNode]: exactly one native [Wallet] per storage
 * directory (the core refuses a second instance over the same one).
 */
class NativeWalletNode(override val wallet: Wallet) : WalletNode {
    override fun start() = wallet.start()

    override fun stop() = wallet.stop()

    override fun restore(mnemonic: String) = wallet.restore(mnemonic)

    override suspend fun runEventLoop(onEvent: suspend (Event) -> Unit) =
        WalletCore.runEventLoop(wallet, onEvent)
}
