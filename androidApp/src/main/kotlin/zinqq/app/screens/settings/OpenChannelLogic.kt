package zinqq.app.screens.settings

import uniffi.wallet_core.OpenFeeEstimate
import uniffi.wallet_core.WalletException
import zinqq.spike.formatBtc

/**
 * OpenChannel's pure derivations (U17, R10 UI, R14): the PWA's bounds and
 * balance gate (`OpenChannel.tsx:29-34,83-111`), the review labels, and the
 * typed-error copy. The core re-enforces the same bounds/balance gate inside
 * `open_channel` — these are the PWA's pre-review inline errors.
 */

/** `MIN_CHANNEL_SATS` (`OpenChannel.tsx:29`). */
const val MIN_CHANNEL_SATS: ULong = 20_000uL

/** `MAX_CHANNEL_SATS` — LDK non-wumbo limit (`OpenChannel.tsx:31`). */
const val MAX_CHANNEL_SATS: ULong = 16_777_215uL

/** `MAX_DIGITS` (`OpenChannel.tsx:32`). */
const val OPEN_AMOUNT_MAX_DIGITS = 8

/**
 * The amount step's gate (`OpenChannel.tsx:83-111`), PWA copy verbatim;
 * `null` = proceed to review.
 */
fun validateOpenAmount(
    amountSats: ULong,
    estimatedFeeSats: ULong,
    balanceSats: ULong,
): String? = when {
    amountSats < MIN_CHANNEL_SATS ->
        "Minimum channel size is ${formatBtc(MIN_CHANNEL_SATS.toLong())}"
    amountSats > MAX_CHANNEL_SATS ->
        "Maximum channel size is ${formatBtc(MAX_CHANNEL_SATS.toLong())}"
    amountSats + estimatedFeeSats > balanceSats ->
        "Amount plus fees exceeds available balance"
    else -> null
}

/** `Est. fee (~{rate} sat/vB)` (`OpenChannel.tsx:263-265`). */
fun openFeeRateLabel(feeRateSatPerVb: ULong): String = "Est. fee (~$feeRateSatPerVb sat/vB)"

/** The review Total row (`OpenChannel.tsx:272`). */
fun openTotalSats(amountSats: ULong, estimatedFeeSats: ULong): ULong =
    amountSats + estimatedFeeSats

/**
 * The PWA's fee-fetch fallback (`OpenChannel.tsx:70-72,97-98`): 1 sat/vB ×
 * the 140 vB approximate funding-tx vsize, used when `estimate_open_fee`
 * fails (e.g. stopped node).
 */
fun fallbackOpenFee(): OpenFeeEstimate =
    OpenFeeEstimate(feeRateSatPerVb = 1uL, estimatedFeeSats = 140uL)

/** `{pubkey.slice(0, 12)}...{pubkey.slice(-8)}` (`OpenChannel.tsx:253`). */
fun reviewPeerDisplay(pubkey: String): String = pubkey.take(12) + "..." + pubkey.takeLast(8)

/**
 * Open-failure copy: the typed channel errors carry the core's PWA-parity
 * strings; unknown failures fall back to the PWA's `ok === false` copy
 * (`OpenChannel.tsx:138-141`).
 */
fun openChannelErrorMessage(e: Throwable): String = when (e) {
    is WalletException.ChannelAmountBelowMinimum ->
        "Minimum channel size is ${formatBtc(MIN_CHANNEL_SATS.toLong())}"
    is WalletException.ChannelAmountAboveMaximum ->
        "Maximum channel size is ${formatBtc(MAX_CHANNEL_SATS.toLong())}"
    is WalletException.ChannelAmountExceedsBalance ->
        "Amount plus fees exceeds available balance"
    is WalletException.InvalidPeerAddress -> e.detail
    is WalletException.PeerConnectFailed -> "Failed to connect to peer: ${e.detail}"
    is WalletException.PeerPersistFailed -> "Failed to persist known peer: ${e.detail}"
    is WalletException.ChannelOpenFailed -> "Failed to initiate channel opening: ${e.detail}"
    is WalletException.NotRunning -> "the node is not running"
    else ->
        e.message?.takeIf { it.isNotBlank() }
            ?: "Failed to initiate channel opening. The peer may have disconnected."
}
