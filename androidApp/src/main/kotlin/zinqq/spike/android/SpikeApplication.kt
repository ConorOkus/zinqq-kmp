package zinqq.spike.android

import android.app.Application

/**
 * Owns the single process-scoped [WalletHolder]. The node holds a tokio runtime,
 * LSP connections, and an exclusive lock on the storage directory, so exactly
 * one may exist per process — see [WalletHolder] for why activity scope was
 * wrong for it.
 */
class SpikeApplication : Application() {
    lateinit var walletHolder: WalletHolder
        private set

    override fun onCreate() {
        super.onCreate()
        walletHolder = WalletHolder(this).also { it.observeProcessLifecycle() }
    }
}
