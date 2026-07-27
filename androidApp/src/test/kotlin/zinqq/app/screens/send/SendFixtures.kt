package zinqq.app.screens.send

import uniffi.wallet_core.ClassifiedKind
import uniffi.wallet_core.ClassifiedView
import uniffi.wallet_core.FeeEstimate
import uniffi.wallet_core.LnurlPayView
import uniffi.wallet_core.MaxSendEstimate

/**
 * Builders over the generated send-flow records (U15) so the step-transition
 * matrices only spell out the fields each case exercises.
 */

fun classifiedView(
    kind: ClassifiedKind = ClassifiedKind.INVALID,
    bolt11: String? = null,
    /** Lowercase hex; the core populates it for BOLT11 targets only (F1). */
    paymentHash: String? = null,
    offer: String? = null,
    amountMsat: ULong? = null,
    description: String? = null,
    address: String? = null,
    amountSats: ULong? = null,
    bip353User: String? = null,
    bip353Domain: String? = null,
    onchainFallbackAddress: String? = null,
    uriAmountSats: ULong? = null,
    error: String? = null,
): ClassifiedView = ClassifiedView(
    kind = kind,
    bolt11 = bolt11,
    paymentHash = paymentHash,
    offer = offer,
    amountMsat = amountMsat,
    description = description,
    address = address,
    amountSats = amountSats,
    bip353User = bip353User,
    bip353Domain = bip353Domain,
    onchainFallbackAddress = onchainFallbackAddress,
    uriAmountSats = uriAmountSats,
    error = error,
)

fun lnurlPayView(
    user: String = "satoshi",
    domain: String = "zinqq.app",
    callback: String = "https://zinqq.app/lnurlp/satoshi/callback",
    minSendableMsat: ULong = 1_000uL,
    maxSendableMsat: ULong = 100_000_000uL,
    minSats: ULong = 1uL,
    maxSats: ULong = 100_000uL,
    skipAmountEntry: Boolean = false,
    description: String = "Pay satoshi@zinqq.app",
    expectedDescriptionHashHex: String? = null,
): LnurlPayView = LnurlPayView(
    user = user,
    domain = domain,
    callback = callback,
    minSendableMsat = minSendableMsat,
    maxSendableMsat = maxSendableMsat,
    minSats = minSats,
    maxSats = maxSats,
    skipAmountEntry = skipAmountEntry,
    description = description,
    expectedDescriptionHashHex = expectedDescriptionHashHex,
)

fun feeEstimate(
    feeSats: ULong = 350uL,
    feeRateSatPerVb: ULong = 2uL,
): FeeEstimate = FeeEstimate(feeSats = feeSats, feeRateSatPerVb = feeRateSatPerVb)

fun maxSendEstimate(
    amountSats: ULong = 40_000uL,
    feeSats: ULong = 500uL,
    feeRateSatPerVb: ULong = 3uL,
    reserveSats: ULong = 10_000uL,
): MaxSendEstimate = MaxSendEstimate(
    amountSats = amountSats,
    feeSats = feeSats,
    feeRateSatPerVb = feeRateSatPerVb,
    reserveSats = reserveSats,
)

/** The hash [TEST_BOLT11] classifies to (lowercase hex, 32 bytes). */
val TEST_PAYMENT_HASH = "aa".repeat(32)

/** Another payment's hash — a previous send still in flight past its cap. */
val OTHER_PAYMENT_HASH = "bb".repeat(32)

const val TEST_ADDRESS = "bc1qw508d6qejxtdg4y5r3zarvary0c5xw7kv8f3t4"
const val TEST_BOLT11 = "lnbc500n1p3xyzzyfakeinvoicefortestsq3sdwj"
const val TEST_OFFER = "lno1qsgqmqvgm96frzdg8m0gc6nzeqffvzsqzrxqy32afmr3jn9ggkwg3egfwch2hy0l6jut6vfd8vpsc3h89l6u3dm4q2d6nuamav3w27xvdmv3lpgklhg7l5teypqz9l53hj7zvuaenh34xqsz2sa967yzqkylfu9xtcd5ymcmfp32h083e805y7jfd236w9afhavqqvl8uyma7x77yun4ehe9pnhu2gekjguexmxpqjcr2j822xr7q34p078gzslf9wpwz5y57alxu99s0z2ql0kfqvwhzycqq45ehh58xnfpuek80hw6spvwrvttjrrq9pphh0dpydh06qqspp5uq4gpyt6n9mwexde44qv7lstzzq60nr40ff38u27un6y53aypmx0p4qruvpuqsnfsmn"
