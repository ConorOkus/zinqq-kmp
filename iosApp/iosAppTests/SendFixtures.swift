import Shared

@testable import iosApp

/// Builders over the generated send-flow records (U20) so the
/// step-transition matrices only spell out the fields each case exercises —
/// the same fixtures as Android's `SendFixtures.kt`.

func classifiedView(
    kind: ClassifiedKind = .invalid,
    bolt11: String? = nil,
    /// Lowercase hex, BOLT11 only (nil for offers/on-chain/BIP353/invalid) —
    /// the hash a send dispatch settles on (F1).
    paymentHash: String? = nil,
    offer: String? = nil,
    amountMsat: UInt64? = nil,
    description: String? = nil,
    address: String? = nil,
    amountSats: UInt64? = nil,
    bip353User: String? = nil,
    bip353Domain: String? = nil,
    onchainFallbackAddress: String? = nil,
    uriAmountSats: UInt64? = nil,
    error: String? = nil
) -> ClassifiedView {
    ClassifiedView(
        kind: kind,
        bolt11: bolt11,
        paymentHash: paymentHash,
        offer: offer,
        amountMsat: amountMsat.map { KotlinULong(unsignedLongLong: $0) },
        description: description,
        address: address,
        amountSats: amountSats.map { KotlinULong(unsignedLongLong: $0) },
        bip353User: bip353User,
        bip353Domain: bip353Domain,
        onchainFallbackAddress: onchainFallbackAddress,
        uriAmountSats: uriAmountSats.map { KotlinULong(unsignedLongLong: $0) },
        error: error
    )
}

func lnurlPayView(
    user: String = "satoshi",
    domain: String = "zinqq.app",
    callback: String = "https://zinqq.app/lnurlp/satoshi/callback",
    minSendableMsat: UInt64 = 1_000,
    maxSendableMsat: UInt64 = 100_000_000,
    minSats: UInt64 = 1,
    maxSats: UInt64 = 100_000,
    skipAmountEntry: Bool = false,
    description: String = "Pay satoshi@zinqq.app",
    expectedDescriptionHashHex: String? = nil
) -> LnurlPayView {
    LnurlPayView(
        user: user,
        domain: domain,
        callback: callback,
        minSendableMsat: minSendableMsat,
        maxSendableMsat: maxSendableMsat,
        minSats: minSats,
        maxSats: maxSats,
        skipAmountEntry: skipAmountEntry,
        description: description,
        expectedDescriptionHashHex: expectedDescriptionHashHex
    )
}

func feeEstimate(
    feeSats: UInt64 = 350,
    feeRateSatPerVb: UInt64 = 2
) -> FeeEstimate {
    FeeEstimate(feeSats: feeSats, feeRateSatPerVb: feeRateSatPerVb)
}

func maxSendEstimate(
    amountSats: UInt64 = 40_000,
    feeSats: UInt64 = 500,
    feeRateSatPerVb: UInt64 = 3,
    reserveSats: UInt64 = 10_000
) -> MaxSendEstimate {
    MaxSendEstimate(
        amountSats: amountSats,
        feeSats: feeSats,
        feeRateSatPerVb: feeRateSatPerVb,
        reserveSats: reserveSats
    )
}

/// The dispatched invoice's payment hash (lowercase hex, as the core emits it).
let ourPaymentHash = String(repeating: "aa", count: 32)

/// Some OTHER payment's hash — e.g. one an earlier send's 5-minute outcome cap
/// abandoned in flight, whose outcome can still land mid-dispatch (F1).
let foreignPaymentHash = String(repeating: "bb", count: 32)

let testAddress = "bc1qw508d6qejxtdg4y5r3zarvary0c5xw7kv8f3t4"
let testBolt11 = "lnbc500n1p3xyzzyfakeinvoicefortestsq3sdwj"
let testOffer = "lno1qsgqmqvgm96frzdg8m0gc6nzeqffvzsqzrxqy32afmr3jn9ggkwg3egfwch2hy0l6jut6vfd8vpsc3h89l6u3dm4q2d6nuamav3w27xvdmv3lpgklhg7l5teypqz9l53hj7zvuaenh34xqsz2sa967yzqkylfu9xtcd5ymcmfp32h083e805y7jfd236w9afhavqqvl8uyma7x77yun4ehe9pnhu2gekjguexmxpqjcr2j822xr7q34p078gzslf9wpwz5y57alxu99s0z2ql0kfqvwhzycqq45ehh58xnfpuek80hw6spvwrvttjrrq9pphh0dpydh06qqspp5uq4gpyt6n9mwexde44qv7lstzzq60nr40ff38u27un6y53aypmx0p4qruvpuqsnfsmn"
