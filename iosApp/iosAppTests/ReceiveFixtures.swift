import Foundation
import Shared

@testable import iosApp

/// Builders over the generated receive-flow records (U21) so the gating and
/// transition matrices only spell out the fields each case exercises — the
/// SAME fixtures as Android's `ReceiveFixtures.kt` (which mirror the PWA's
/// `Receive.test.tsx` `makeQuote`/`mockChannel`/ready-context bundle).

let testReceiveAddress = "bc1qw508d6qejxtdg4y5r3zarvary0c5xw7kxpjzsx"
let testReceiveBolt11 = "lnbc1fakeinvoice"
let testPaymentHash = "abc123"
let testReceiveOffer = "lno1fakeoffer"
let testAsyncReceiveOffer = "lno1fakeasyncoffer"

func makeBundle(
    address: String = testReceiveAddress,
    bolt11: String? = testReceiveBolt11,
    paymentHash: String? = testPaymentHash,
    invoiceError: String? = nil,
    bip321Uri: String? = nil,
    qrValue: String? = nil,
    offer: String? = nil,
    offerQrValue: String? = nil,
    needsJit: Bool = false,
    minReceiveSats: UInt64 = 3_000
) -> ReceiveBundle {
    let uri = bip321Uri
        ?? "bitcoin:\(address.uppercased())?lightning=\(bolt11 ?? "null")"
    return ReceiveBundle(
        address: address,
        bolt11: bolt11,
        paymentHash: paymentHash,
        invoiceError: invoiceError,
        bip321Uri: uri,
        qrValue: qrValue ?? uri.uppercased(),
        offer: offer,
        offerQrValue: offerQrValue ?? offer.map { "bitcoin:?lno=\($0)".uppercased() },
        needsJit: needsJit,
        minReceiveSats: minReceiveSats
    )
}

func makeQuote(
    quoteToken: UInt64 = 1,
    amountMsat: UInt64 = 10_000_000,
    openingFeeMsat: UInt64 = 2_500_000,
    validUntilUnix: UInt64 = 1_700_000_300,
    freshEnough: Bool = true
) -> JitQuote {
    JitQuote(
        quoteToken: quoteToken,
        amountMsat: amountMsat,
        openingFeeMsat: openingFeeMsat,
        receiveMsat: amountMsat - openingFeeMsat,
        validUntilUnix: validUntilUnix,
        freshEnough: freshEnough
    )
}

func makeJitInvoice(
    bolt11: String = "lnbc1fakejitinvoice",
    paymentHash: String = testPaymentHash,
    openingFeeMsat: UInt64 = 2_500_000,
    expiresAtUnix: UInt64 = 1_700_000_270
) -> JitInvoice {
    JitInvoice(
        bolt11: bolt11,
        paymentHash: paymentHash,
        openingFeeMsat: openingFeeMsat,
        expiresAtUnix: expiresAtUnix
    )
}

/// PWA `mockChannel(inboundCapacityMsat, isUsable)`.
func makeChannel(
    inboundMsat: UInt64,
    usable: Bool = true
) -> ChannelView {
    ChannelView(
        channelId: String(repeating: "00", count: 32),
        counterpartyPubkey: "02" + String(repeating: "00", count: 32),
        state: usable ? .active : .ready,
        capacitySats: 1_000_000,
        outboundMsat: 500_000_000,
        inboundMsat: inboundMsat,
        reserveSats: nil,
        usable: usable,
        pendingHtlcCount: 0
    )
}

/// Kotlin exceptions cross the Kotlin/Native bridge wrapped in an NSError
/// with the throwable under `KotlinException` — this reproduces that bridge
/// shape so the classify functions see what a real FFI throw delivers.
func kotlinError(_ exception: WalletException) -> NSError {
    NSError(
        domain: "KotlinException",
        code: 0,
        userInfo: [
            "KotlinException": exception,
            NSLocalizedDescriptionKey: exception.message ?? "wallet error",
        ]
    )
}
