import Foundation
import Shared

/// The settings suite's window onto the wallet (U22, R14): every call is a
/// thin passthrough to the core FFI — peer/channel management, fee
/// estimates, and the mnemonic reveal all happen in Rust. `WalletModel`
/// implements this; tests can fake it. Mirrors Android's `SettingsPort`.
@MainActor
protocol SettingsPort: AnyObject {
    /// The stored 12 words (R1); the 60 s auto-hide is UI policy here.
    func revealMnemonic() async throws -> String

    /// BIP39 validity of a candidate restore mnemonic, via the core's
    /// `deriveDebugInfo` — the only exported call that checks it (FFI note:
    /// there is no dedicated validate export).
    func validateMnemonic(_ mnemonic: String) async -> Bool

    func listPeers() async throws -> [PeerView]
    func listChannels() async throws -> [ChannelView]

    /// Fails typed with `PeerHasOpenChannels` while channels are open (R10).
    func forgetPeer(pubkey: String) async throws

    /// Connect-if-needed + `create_channel`, like the PWA's OpenChannel.
    func openChannel(peerAddress: String, amountSats: UInt64) async throws -> String

    func estimateOpenFee() async throws -> OpenFeeEstimate

    /// Informational only — never fails; all-nil when unknown (R10).
    func estimateClose(channelId: String) async throws -> CloseEstimate

    func closeChannel(channelId: String, force: Bool) async throws

    /// Trusted-spendable on-chain sats — OpenChannel's "available" line.
    func onchainBalanceSats() -> UInt64
}

/// The general Kotlin-throwable unwrap: Kotlin exceptions cross the
/// Kotlin/Native bridge as NSError with the original throwable under
/// `KotlinException` (the typed-`WalletException` unwrap in `SendFlow.swift`
/// is the narrow version of this).
func kotlinThrowable(_ error: Error) -> KotlinThrowable? {
    (error as NSError).userInfo["KotlinException"] as? KotlinThrowable
}
