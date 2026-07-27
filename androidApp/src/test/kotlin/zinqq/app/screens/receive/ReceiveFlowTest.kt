package zinqq.app.screens.receive

import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertFalse
import kotlin.test.assertIs
import kotlin.test.assertNull
import kotlin.test.assertTrue
import uniffi.wallet_core.Event
import uniffi.wallet_core.WalletException

/**
 * The receive machine's gating and transition matrices (U16): floor gating
 * (AE4), the needs-JIT presentation decision, quote staleness → re-quote,
 * expiry flip + mid-edit suppression, received settle, pager eligibility,
 * caption derivation, and countdown formatting — all mirroring the PWA's
 * `Receive.tsx` / `Receive.test.tsx`.
 */
class ReceiveFlowTest {

    private val floor = 3_000uL

    // --- floor gating matrix (AE4, PWA Receive.tsx:133-134, test:576-604) ---

    @Test
    fun belowFloorJitAmountIsBlocked() {
        // No capacity: every sat needs JIT.
        assertTrue(belowJitMinimum(editingNeedsJit(0uL, 2_999uL), 2_999uL, floor))
        assertEquals(
            ConfirmDecision.Blocked,
            confirmAmount(2_999uL, usableInboundMsat = 0uL, floorSats = floor),
        )
    }

    @Test
    fun atFloorJitAmountPasses() {
        assertFalse(belowJitMinimum(editingNeedsJit(0uL, 3_000uL), 3_000uL, floor))
        val decision = confirmAmount(3_000uL, usableInboundMsat = 0uL, floorSats = floor)
        assertEquals(ConfirmDecision.Request(3_000uL, presentQuoting = true), decision)
    }

    @Test
    fun aboveFloorJitAmountPasses() {
        val decision = confirmAmount(50_000uL, usableInboundMsat = 0uL, floorSats = floor)
        assertEquals(ConfirmDecision.Request(50_000uL, presentQuoting = true), decision)
    }

    @Test
    fun belowFloorAmountCoveredByCapacityIsNotGated() {
        // In-capacity receives are unaffected by the JIT floor (AE4 scope).
        val inbound = 10_000uL * 1_000uL
        assertFalse(belowJitMinimum(editingNeedsJit(inbound, 500uL), 500uL, floor))
        assertEquals(
            ConfirmDecision.Request(500uL, presentQuoting = false),
            confirmAmount(500uL, usableInboundMsat = inbound, floorSats = floor),
        )
    }

    @Test
    fun zeroAmountRaisesNoAlertAndDisablesNext() {
        assertFalse(belowJitMinimum(editingNeedsJit(0uL, 0uL), 0uL, floor))
        assertFalse(numpadNextEnabled(0uL, belowMinimum = false))
    }

    @Test
    fun belowMinimumDisablesNext() {
        assertFalse(numpadNextEnabled(2_999uL, belowMinimum = true))
        assertTrue(numpadNextEnabled(3_000uL, belowMinimum = false))
    }

    @Test
    fun minimumAlertCarriesThePwaCopy() {
        assertEquals("Minimum ₿3,000", minimumAlertText(3_000uL))
    }

    @Test
    fun liveFloorRaisesTheGateAboveTheStaticFloor() {
        // PWA test:650: live LSP minimum above the static floor governs.
        val liveFloor = 5_000uL
        assertTrue(belowJitMinimum(editingNeedsJit(0uL, 4_000uL), 4_000uL, liveFloor))
    }

    // --- needs-JIT decision presentation (PWA Receive.tsx:425-439) ---

    @Test
    fun jitConfirmPresentsTheQuotingSkeletonImmediately() {
        // Inbound covers 5,000 sats; asking 6,000 needs JIT.
        val decision = confirmAmount(6_000uL, 5_000_000uL, floor)
        assertEquals(ConfirmDecision.Request(6_000uL, presentQuoting = true), decision)
    }

    @Test
    fun inCapacityConfirmDoesNotPresentQuoting() {
        val decision = confirmAmount(5_000uL, 5_000_000uL, floor)
        assertEquals(ConfirmDecision.Request(5_000uL, presentQuoting = false), decision)
    }

    @Test
    fun exactCapacityBoundaryNeedsJit() {
        // needs_jit is `inbound < amount * 1000` — equality is servable.
        assertFalse(editingNeedsJit(5_000_000uL, 5_000uL))
        assertTrue(editingNeedsJit(4_999_999uL, 5_000uL))
    }

    // --- usable inbound sum (PWA Receive.tsx:33-39) ---

    @Test
    fun usableInboundSumsOnlyUsableChannels() {
        val channels = listOf(
            makeChannel(inboundMsat = 1_000_000uL, usable = true),
            makeChannel(inboundMsat = 2_000_000uL, usable = false),
            makeChannel(inboundMsat = 3_000_000uL, usable = true),
        )
        assertEquals(4_000_000uL, usableInboundMsat(channels))
        assertEquals(0uL, usableInboundMsat(emptyList()))
    }

    // --- review derivation (PWA Receive.tsx:726-751) ---

    @Test
    fun reviewRowsCeilTheFeeAndDeriveTheNet() {
        val review = ReceiveStep.JitReview(
            amountSats = 10_000uL,
            quote = makeQuote(amountMsat = 10_000_000uL, openingFeeMsat = 2_500_001uL),
        )
        // (2_500_001 + 999) / 1000 = 2501 — the PWA's ceil.
        assertEquals(2_501uL, review.setupFeeSats)
        assertEquals(7_499uL, review.youReceiveSats)
    }

    // --- quote staleness → re-quote (PWA Receive.tsx:534-537) ---

    @Test
    fun staleQuoteAtBuyDemandsAReQuote() {
        assertEquals(
            BuyFailure.RE_QUOTE,
            classifyBuyFailure(WalletException.JitReQuoteRequired()),
        )
    }

    @Test
    fun otherBuyFailuresGoToTheErrorScreen() {
        assertEquals(
            BuyFailure.ERROR,
            classifyBuyFailure(WalletException.Lsps2("lsps2.buy failed: boom")),
        )
        assertEquals(BuyFailure.ERROR, classifyBuyFailure(RuntimeException("network")))
    }

    // --- quote failure classification (PWA Receive.tsx:249-268) ---

    @Test
    fun belowMinimumQuoteFailureIsRecognisedFromTheCoreCopy() {
        val e = WalletException.Lsps2(
            "LSPS2 request failed: amount 500000msat is below the LSP minimum " +
                "payment size of 3000000msat",
        )
        assertEquals(QuoteFailure.BelowMinimum(3_000_000uL), classifyQuoteFailure(e))
    }

    @Test
    fun otherQuoteFailuresFallBackToOnchain() {
        assertEquals(
            QuoteFailure.Other,
            classifyQuoteFailure(WalletException.Lsps2("lsps2.get_info failed: boom")),
        )
        assertEquals(QuoteFailure.Other, classifyQuoteFailure(RuntimeException("nope")))
    }

    // --- expiry flip + suppression (PWA Receive.tsx:319-330, 814-818) ---

    @Test
    fun displayedJitInvoiceFlipsToExpired() {
        assertEquals(
            ReceiveStep.JitExpired,
            applyExpiryFlip(ReceiveStep.Display(InvoicePath.JIT)),
        )
    }

    @Test
    fun onlyTheJitQrFlips() {
        val standard = ReceiveStep.Display(InvoicePath.STANDARD)
        assertEquals(standard, applyExpiryFlip(standard))
        val review = ReceiveStep.JitReview(10_000uL, makeQuote())
        assertEquals(review, applyExpiryFlip(review))
        val received = ReceiveStep.Received(10_000uL)
        assertEquals(received, applyExpiryFlip(received))
    }

    @Test
    fun expiredScreenIsSuppressedMidEdit() {
        // PWA test:431: expiry mid-edit keeps the numpad; Cancel lands on it.
        assertFalse(showExpiredScreen(ReceiveStep.JitExpired, editingAmount = true))
        assertTrue(showExpiredScreen(ReceiveStep.JitExpired, editingAmount = false))
        assertFalse(showExpiredScreen(ReceiveStep.Display(InvoicePath.JIT), false))
    }

    @Test
    fun countdownIsSuppressedWhileEditing() {
        val jit = ReceiveStep.Display(InvoicePath.JIT)
        assertTrue(countdownVisible(jit, editingAmount = false, expiresAtUnix = 1_700uL))
        assertFalse(countdownVisible(jit, editingAmount = true, expiresAtUnix = 1_700uL))
        assertFalse(countdownVisible(jit, editingAmount = false, expiresAtUnix = null))
        assertFalse(
            countdownVisible(
                ReceiveStep.Display(InvoicePath.STANDARD),
                editingAmount = false,
                expiresAtUnix = 1_700uL,
            ),
        )
    }

    // --- countdown math + formatting (R6) ---

    @Test
    fun countdownIsExpiryMinusNowFlooredAtZero() {
        assertEquals(576L, countdownSecondsLeft(1_700_000_576uL, 1_700_000_000L))
        assertEquals(0L, countdownSecondsLeft(1_700_000_000uL, 1_700_000_000L))
        assertEquals(0L, countdownSecondsLeft(1_699_999_000uL, 1_700_000_000L))
    }

    @Test
    fun countdownFormatsMinutesAndPaddedSeconds() {
        assertEquals("Expires in 9:36", countdownText(576))
        assertEquals("Expires in 0:59", countdownText(59))
        assertEquals("Expires in 0:00", countdownText(0))
        assertEquals("Expires in 60:00", countdownText(3_600))
    }

    // --- received settle (PWA Receive.tsx:332-343) ---

    @Test
    fun matchingPaymentReceivedSettlesWithTheFlooredAmount() {
        val settled = applyPaymentReceived(
            awaitedPaymentHash = TEST_PAYMENT_HASH,
            event = Event.PaymentReceived(
                paymentHash = TEST_PAYMENT_HASH,
                amountMsat = 12_345_678uL,
                skimmedFeeMsat = 2_500_000uL,
            ),
        )
        assertEquals(ReceiveStep.Received(12_345uL), settled)
    }

    @Test
    fun mismatchedOrAbsentHashDoesNotSettle() {
        val event = Event.PaymentReceived(
            paymentHash = "other",
            amountMsat = 1_000uL,
            skimmedFeeMsat = null,
        )
        assertNull(applyPaymentReceived(TEST_PAYMENT_HASH, event))
        assertNull(applyPaymentReceived(null, event))
    }

    @Test
    fun nonReceiveEventsDoNotSettle() {
        assertNull(
            applyPaymentReceived(
                TEST_PAYMENT_HASH,
                Event.InvoiceReady(bolt11 = "lnbc1", expiryUnixSecs = 0uL),
            ),
        )
    }

    // --- pager eligibility (PWA Receive.tsx:372, R6) ---

    @Test
    fun offerPageNeedsAnOfferAndAUsableChannel() {
        // The core only emits an offer with ≥1 usable channel, so
        // offerExists encodes that; needsAmount is the no-channel visit.
        assertTrue(showBolt12Page(offerExists = true, needsAmount = false))
        assertFalse(showBolt12Page(offerExists = false, needsAmount = false))
        assertFalse(showBolt12Page(offerExists = true, needsAmount = true))
        assertFalse(showBolt12Page(offerExists = false, needsAmount = true))
    }

    // --- caption + copy derivation (PWA Receive.tsx:993-1001, 1027-1029) ---

    @Test
    fun captionsFollowThePageAndInvoicePath() {
        assertEquals(
            "Reusable QR code",
            qrCaption(QrPage.BOLT12, InvoicePath.STANDARD, openingFeeSats = null),
        )
        assertEquals(
            "Setup fee: ₿2,500",
            qrCaption(QrPage.UNIFIED, InvoicePath.JIT, openingFeeSats = 2_500uL),
        )
        assertEquals(
            "Request money by letting someone scan this QR code",
            qrCaption(QrPage.UNIFIED, InvoicePath.STANDARD, openingFeeSats = null),
        )
        assertEquals(
            "Request money by letting someone scan this QR code",
            qrCaption(QrPage.UNIFIED, InvoicePath.NONE, openingFeeSats = null),
        )
    }

    @Test
    fun copySheetTitleFollowsThePage() {
        assertEquals("Reusable payment request", copySheetTitle(QrPage.BOLT12))
        assertEquals("Payment request", copySheetTitle(QrPage.UNIFIED))
    }

    @Test
    fun copyValueUsesTheLowercaseLnoFormOnTheOfferPage() {
        val uri = "bitcoin:BC1Q?lightning=lnbc1"
        assertEquals(
            "bitcoin:?lno=$TEST_RECEIVE_OFFER",
            copyValue(QrPage.BOLT12, uri, TEST_RECEIVE_OFFER),
        )
        assertEquals(uri, copyValue(QrPage.UNIFIED, uri, TEST_RECEIVE_OFFER))
        // No offer: the bolt12 page cannot exist, fall back to the URI.
        assertEquals(uri, copyValue(QrPage.BOLT12, uri, null))
    }

    // --- numpad CTA + header copy (PWA Receive.tsx:928, 642-649) ---

    @Test
    fun mandatoryFirstAmountUsesTheRequestCta() {
        assertEquals("Request", numpadCtaLabel(needsAmount = true, confirmedAmountSats = 0uL))
        assertEquals("Done", numpadCtaLabel(needsAmount = true, confirmedAmountSats = 500uL))
        assertEquals("Done", numpadCtaLabel(needsAmount = false, confirmedAmountSats = 0uL))
    }

    @Test
    fun headerCopyOnlyShowsOverTheQr() {
        val display = ReceiveStep.Display(InvoicePath.STANDARD)
        assertTrue(headerCopyVisible(hasAddress = true, editingAmount = false, step = display))
        assertFalse(headerCopyVisible(hasAddress = false, editingAmount = false, step = display))
        assertFalse(headerCopyVisible(hasAddress = true, editingAmount = true, step = display))
        assertFalse(headerCopyVisible(true, false, ReceiveStep.Quoting))
        assertFalse(headerCopyVisible(true, false, ReceiveStep.JitReview(1uL, makeQuote())))
        assertFalse(headerCopyVisible(true, false, ReceiveStep.Buying))
        assertFalse(headerCopyVisible(true, false, ReceiveStep.JitExpired))
        assertFalse(headerCopyVisible(true, false, ReceiveStep.JitError))
    }

    // --- fixture sanity: the step type used by the controller matrix ---

    @Test
    fun bundleFixtureCarriesTheJitDecision() {
        val bundle = makeBundle(needsJit = true, bolt11 = null, paymentHash = null)
        assertTrue(bundle.needsJit)
        assertNull(bundle.bolt11)
        assertIs<ReceiveStep.Display>(ReceiveStep.Display(InvoicePath.NONE))
    }
}
