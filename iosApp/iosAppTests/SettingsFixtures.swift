import Shared

@testable import iosApp

/// Builders over the U9 channel/peer uniffi records so the U22 presentation
/// matrices only spell out the fields each case exercises — the same
/// fixtures as Android's `SettingsFixtures.kt`.

let peerPubkey = "02" + String(repeating: "ab", count: 32)

func peerView(
    pubkey: String = peerPubkey,
    address: String? = "203.0.113.9:9735",
    connected: Bool = false,
    known: Bool = true,
    channelCount: UInt32 = 0
) -> PeerView {
    PeerView(
        pubkey: pubkey,
        address: address,
        connected: connected,
        known: known,
        channelCount: channelCount
    )
}

func channelView(
    channelId: String = String(repeating: "cc", count: 32),
    counterpartyPubkey: String = peerPubkey,
    state: ChannelStateLabel = .active,
    capacitySats: UInt64 = 100_000,
    outboundMsat: UInt64 = 60_000_000,
    inboundMsat: UInt64 = 40_000_000,
    reserveSats: UInt64? = nil,
    usable: Bool = true,
    pendingHtlcCount: UInt32 = 0
) -> ChannelView {
    ChannelView(
        channelId: channelId,
        counterpartyPubkey: counterpartyPubkey,
        state: state,
        capacitySats: capacitySats,
        outboundMsat: outboundMsat,
        inboundMsat: inboundMsat,
        reserveSats: reserveSats.map { KotlinULong(unsignedLongLong: $0) },
        usable: usable,
        pendingHtlcCount: pendingHtlcCount
    )
}

func closeEstimate(
    feePayer: CloseFeePayer = .unknown,
    coopCloseFeeSats: UInt64? = nil,
    commitmentFeeSats: UInt64? = nil,
    cpfpFeeSats: UInt64? = nil,
    sweepFeeSats: UInt64? = nil,
    coopTotalYouPaySats: UInt64? = nil,
    forceTotalYouPaySats: UInt64? = nil,
    expectedBackSats: UInt64? = nil,
    timelockBlocks: UInt16? = nil,
    pendingHtlcCount: UInt32? = nil,
    isAnchor: Bool? = nil
) -> CloseEstimate {
    CloseEstimate(
        feePayer: feePayer,
        coopCloseFeeSats: coopCloseFeeSats.map { KotlinULong(unsignedLongLong: $0) },
        commitmentFeeSats: commitmentFeeSats.map { KotlinULong(unsignedLongLong: $0) },
        cpfpFeeSats: cpfpFeeSats.map { KotlinULong(unsignedLongLong: $0) },
        sweepFeeSats: sweepFeeSats.map { KotlinULong(unsignedLongLong: $0) },
        coopTotalYouPaySats: coopTotalYouPaySats.map { KotlinULong(unsignedLongLong: $0) },
        forceTotalYouPaySats: forceTotalYouPaySats.map { KotlinULong(unsignedLongLong: $0) },
        expectedBackSats: expectedBackSats.map { KotlinULong(unsignedLongLong: $0) },
        timelockBlocks: timelockBlocks.map { KotlinUShort(unsignedShort: $0) },
        pendingHtlcCount: pendingHtlcCount.map { KotlinUInt(unsignedInt: $0) },
        isAnchor: isAnchor.map { KotlinBoolean(bool: $0) }
    )
}
