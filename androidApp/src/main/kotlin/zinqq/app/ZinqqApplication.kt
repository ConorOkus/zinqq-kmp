package zinqq.app

import android.app.Application
import zinqq.app.theme.SettingsRepository

/**
 * Owns the single process-scoped [WalletHolder]. The node holds a tokio runtime,
 * LSP connections, and an exclusive lock on the storage directory, so exactly
 * one may exist per process — see [WalletHolder] for why activity scope was
 * wrong for it.
 */
class ZinqqApplication : Application() {
    lateinit var walletHolder: WalletHolder
        private set

    /** UI preferences (appearance mode, balance visibility) — U13, KTD-11. */
    lateinit var settings: SettingsRepository
        private set

    override fun onCreate() {
        super.onCreate()
        settings = SettingsRepository(this)
        walletHolder = WalletHolder(this, settings).also { it.observeProcessLifecycle() }
    }
}
