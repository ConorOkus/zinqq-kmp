package zinqq.spike

// The uniffi.wallet_core package is generated at build time by the Gobley
// uniffi plugin (library mode) from the wallet-core cdylib; UniFFI lower-camels
// the exported Rust fn names (core_version -> coreVersion).
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
}
