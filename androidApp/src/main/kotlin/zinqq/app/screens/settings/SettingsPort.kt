package zinqq.app.screens.settings

import uniffi.wallet_core.ChannelView
import uniffi.wallet_core.CloseEstimate
import uniffi.wallet_core.OpenFeeEstimate
import uniffi.wallet_core.PeerView

/**
 * The settings suite's window onto the wallet (U17, R14): every call is a
 * thin IO-dispatched passthrough to the core FFI — peer/channel management,
 * fee estimates, and the mnemonic reveal all happen in Rust.
 * [WalletHolder][zinqq.app.WalletHolder] implements this; tests can fake it.
 */
interface SettingsPort {
    /** The stored 12 words (R1); the 60 s auto-hide is UI policy here. */
    suspend fun revealMnemonic(): String

    /**
     * BIP39 validity of a candidate restore mnemonic, via the core's
     * `derive_debug_info` — the only exported call that checks it (FFI note:
     * there is no dedicated validate export).
     */
    suspend fun validateMnemonic(mnemonic: String): Boolean

    suspend fun listPeers(): List<PeerView>
    suspend fun listChannels(): List<ChannelView>

    /** Fails typed with `PeerHasOpenChannels` while channels are open (R10). */
    suspend fun forgetPeer(pubkey: String)

    /** Connect-if-needed + `create_channel`, like the PWA's OpenChannel. */
    suspend fun openChannel(peerAddress: String, amountSats: ULong): String

    suspend fun estimateOpenFee(): OpenFeeEstimate

    /** Informational only — never fails; all-null when unknown (R10). */
    suspend fun estimateClose(channelId: String): CloseEstimate

    suspend fun closeChannel(channelId: String, force: Boolean)

    /** Trusted-spendable on-chain sats — OpenChannel's "available" line. */
    fun onchainBalanceSats(): ULong
}
