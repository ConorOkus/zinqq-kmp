package zinqq.app.screens.receive

import uniffi.wallet_core.ChannelStateLabel
import uniffi.wallet_core.ChannelView
import uniffi.wallet_core.JitInvoice
import uniffi.wallet_core.JitQuote
import uniffi.wallet_core.ReceiveBundle

/**
 * Builders over the generated receive-flow records (U16) so the gating and
 * transition matrices only spell out the fields each case exercises. Shapes
 * mirror the PWA's `Receive.test.tsx` fixtures (`makeQuote`, `mockChannel`,
 * the ready-context bundle).
 */

const val TEST_RECEIVE_ADDRESS = "bc1qw508d6qejxtdg4y5r3zarvary0c5xw7kxpjzsx"
const val TEST_RECEIVE_BOLT11 = "lnbc1fakeinvoice"
const val TEST_PAYMENT_HASH = "abc123"
const val TEST_RECEIVE_OFFER = "lno1fakeoffer"

fun makeBundle(
    address: String = TEST_RECEIVE_ADDRESS,
    bolt11: String? = TEST_RECEIVE_BOLT11,
    paymentHash: String? = TEST_PAYMENT_HASH,
    invoiceError: String? = null,
    bip321Uri: String = "bitcoin:${address.uppercase()}?lightning=$bolt11",
    qrValue: String = bip321Uri.uppercase(),
    offer: String? = null,
    offerQrValue: String? = offer?.let { "bitcoin:?lno=$it".uppercase() },
    needsJit: Boolean = false,
    minReceiveSats: ULong = 3_000uL,
): ReceiveBundle = ReceiveBundle(
    address = address,
    bolt11 = bolt11,
    paymentHash = paymentHash,
    invoiceError = invoiceError,
    bip321Uri = bip321Uri,
    qrValue = qrValue,
    offer = offer,
    offerQrValue = offerQrValue,
    needsJit = needsJit,
    minReceiveSats = minReceiveSats,
)

fun makeQuote(
    quoteToken: ULong = 1uL,
    amountMsat: ULong = 10_000_000uL,
    openingFeeMsat: ULong = 2_500_000uL,
    validUntilUnix: ULong = 1_700_000_300uL,
    freshEnough: Boolean = true,
): JitQuote = JitQuote(
    quoteToken = quoteToken,
    amountMsat = amountMsat,
    openingFeeMsat = openingFeeMsat,
    receiveMsat = amountMsat - openingFeeMsat,
    validUntilUnix = validUntilUnix,
    freshEnough = freshEnough,
)

fun makeJitInvoice(
    bolt11: String = "lnbc1fakejitinvoice",
    paymentHash: String = TEST_PAYMENT_HASH,
    openingFeeMsat: ULong = 2_500_000uL,
    expiresAtUnix: ULong = 1_700_000_270uL,
): JitInvoice = JitInvoice(
    bolt11 = bolt11,
    paymentHash = paymentHash,
    openingFeeMsat = openingFeeMsat,
    expiresAtUnix = expiresAtUnix,
)

/** PWA `mockChannel(inboundCapacityMsat, isUsable)`. */
fun makeChannel(
    inboundMsat: ULong,
    usable: Boolean = true,
): ChannelView = ChannelView(
    channelId = "00".repeat(32),
    counterpartyPubkey = "02" + "00".repeat(32),
    state = if (usable) ChannelStateLabel.ACTIVE else ChannelStateLabel.READY,
    capacitySats = 1_000_000uL,
    outboundMsat = 500_000_000uL,
    inboundMsat = inboundMsat,
    reserveSats = null,
    usable = usable,
    pendingHtlcCount = 0u,
)
