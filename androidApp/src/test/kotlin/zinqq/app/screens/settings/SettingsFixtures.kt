package zinqq.app.screens.settings

import uniffi.wallet_core.ChannelStateLabel
import uniffi.wallet_core.ChannelView
import uniffi.wallet_core.CloseEstimate
import uniffi.wallet_core.CloseFeePayer
import uniffi.wallet_core.PeerView

/**
 * Builders over the U9 channel/peer uniffi records so the U17 presentation
 * matrices only spell out the fields each case exercises.
 */

const val PEER_PUBKEY: String =
    "02abababababababababababababababababababababababababababababababab"

fun peerView(
    pubkey: String = PEER_PUBKEY,
    address: String? = "203.0.113.9:9735",
    connected: Boolean = false,
    known: Boolean = true,
    channelCount: UInt = 0u,
): PeerView = PeerView(
    pubkey = pubkey,
    address = address,
    connected = connected,
    known = known,
    channelCount = channelCount,
)

fun channelView(
    channelId: String = "cc".repeat(32),
    counterpartyPubkey: String = PEER_PUBKEY,
    state: ChannelStateLabel = ChannelStateLabel.ACTIVE,
    capacitySats: ULong = 100_000uL,
    outboundMsat: ULong = 60_000_000uL,
    inboundMsat: ULong = 40_000_000uL,
    reserveSats: ULong? = null,
    usable: Boolean = true,
    pendingHtlcCount: UInt = 0u,
): ChannelView = ChannelView(
    channelId = channelId,
    counterpartyPubkey = counterpartyPubkey,
    state = state,
    capacitySats = capacitySats,
    outboundMsat = outboundMsat,
    inboundMsat = inboundMsat,
    reserveSats = reserveSats,
    usable = usable,
    pendingHtlcCount = pendingHtlcCount,
)

fun closeEstimate(
    feePayer: CloseFeePayer = CloseFeePayer.UNKNOWN,
    coopCloseFeeSats: ULong? = null,
    commitmentFeeSats: ULong? = null,
    cpfpFeeSats: ULong? = null,
    sweepFeeSats: ULong? = null,
    coopTotalYouPaySats: ULong? = null,
    forceTotalYouPaySats: ULong? = null,
    expectedBackSats: ULong? = null,
    timelockBlocks: UShort? = null,
    pendingHtlcCount: UInt? = null,
    isAnchor: Boolean? = null,
): CloseEstimate = CloseEstimate(
    feePayer = feePayer,
    coopCloseFeeSats = coopCloseFeeSats,
    commitmentFeeSats = commitmentFeeSats,
    cpfpFeeSats = cpfpFeeSats,
    sweepFeeSats = sweepFeeSats,
    coopTotalYouPaySats = coopTotalYouPaySats,
    forceTotalYouPaySats = forceTotalYouPaySats,
    expectedBackSats = expectedBackSats,
    timelockBlocks = timelockBlocks,
    pendingHtlcCount = pendingHtlcCount,
    isAnchor = isAnchor,
)
