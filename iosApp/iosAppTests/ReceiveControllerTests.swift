import Shared
import XCTest

@testable import iosApp

/// The controller's async transitions over a fake port (U21): mandatory
/// amount entry, quote → review → buy → JIT QR, staleness re-quote,
/// below-minimum review, the expiry flip, and the settlement watcher — the
/// SAME fixtures as Android's `ReceiveControllerTest`.
final class ReceiveControllerTests: XCTestCase {

    // MARK: Fake port

    @MainActor
    private final class FakeReceivePort: ReceivePort {
        var bundleFor: (UInt64?) throws -> ReceiveBundle = { _ in makeBundle() }
        var quoteFor: (UInt64) throws -> JitQuote = { makeQuote(amountMsat: $0) }
        var acceptFor: (UInt64, UInt64) throws -> JitInvoice = { _, _ in makeJitInvoice() }
        var floorSats: UInt64 = 3_000
        var inboundMsat: UInt64 = 0
        var floorFetches = 0

        private var continuations: [AsyncStream<WalletEvent>.Continuation] = []

        func receiveBundle(amountMsat: UInt64?) async throws -> ReceiveBundle {
            try bundleFor(amountMsat)
        }

        func jitQuote(amountMsat: UInt64) async throws -> JitQuote {
            try quoteFor(amountMsat)
        }

        func jitAccept(quoteToken: UInt64, amountMsat: UInt64) async throws -> JitInvoice {
            try acceptFor(quoteToken, amountMsat)
        }

        func minReceiveSats(refresh: Bool) async throws -> UInt64 {
            floorFetches += 1
            return floorSats
        }

        func usableInboundMsat() async throws -> UInt64 { inboundMsat }

        func buildUnifiedUri(
            address: String,
            amountSats: UInt64?,
            invoice: String?
        ) async throws -> String {
            var uri = "bitcoin:\(address.uppercased())"
            if let invoice { uri += "?lightning=\(invoice)" }
            return uri
        }

        /// Fresh subscription per access, registered synchronously — the same
        /// contract as `WalletModel.walletEvents`.
        var walletEvents: AsyncStream<WalletEvent> {
            AsyncStream { continuation in
                continuations.append(continuation)
            }
        }

        func emit(_ event: WalletEvent) {
            for continuation in continuations {
                continuation.yield(event)
            }
        }
    }

    /// No channels: amountless bundle needs JIT, no bolt11, no offer.
    @MainActor
    private func freshWalletPort() -> FakeReceivePort {
        let port = FakeReceivePort()
        port.bundleFor = { _ in
            makeBundle(
                bolt11: nil,
                paymentHash: nil,
                bip321Uri: "bitcoin:\(testReceiveAddress.uppercased())",
                needsJit: true
            )
        }
        return port
    }

    /// A hand-cranked expiry timer: the controller's injected `sleepMs`
    /// suspends here until the test calls `fire()` — the virtual-time stand-in
    /// for Android's `advanceTimeBy`.
    @MainActor
    private final class GatedSleep {
        private(set) var requestedMs: UInt64?
        private var waiters: [CheckedContinuation<Void, Never>] = []

        func sleep(_ ms: UInt64) async {
            requestedMs = ms
            await withCheckedContinuation { waiters.append($0) }
        }

        func fire() {
            let pending = waiters
            waiters = []
            for waiter in pending { waiter.resume() }
        }
    }

    // MARK: Harness

    /// Spin the main-actor cooperative pool until `condition` holds (the
    /// stand-in for the Kotlin test scheduler's `advanceUntilIdle`).
    @MainActor
    private func waitUntil(
        timeout: TimeInterval = 5,
        _ condition: () -> Bool
    ) async {
        let deadline = Date().addingTimeInterval(timeout)
        while !condition() && Date() < deadline {
            await Task.yield()
            try? await Task.sleep(nanoseconds: 1_000_000)
        }
    }

    /// Build and start a controller over `port`, waiting for entry to settle.
    @MainActor
    private func startController(
        _ port: FakeReceivePort,
        sleepMs: @escaping (UInt64) async throws -> Void = { _ in }
    ) async -> ReceiveController {
        let controller = ReceiveController(port: port, nowUnixSecs: { 0 }, sleepMs: sleepMs)
        controller.start()
        await waitUntil { !controller.state.loading || controller.state.loadError != nil }
        return controller
    }

    @MainActor
    private func enterAmount(_ controller: ReceiveController, _ digits: String) {
        for char in digits {
            controller.onNumpadKey(.digit(char))
        }
        controller.confirmAmount()
    }

    // MARK: Tests

    @MainActor
    func testFreshWalletEntryOpensTheMandatoryNumpad() async {
        let c = await startController(freshWalletPort())
        let state = c.state
        XCTAssertFalse(state.loading)
        XCTAssertTrue(state.needsAmount)
        XCTAssertTrue(state.editingAmount)
        XCTAssertEqual(
            "Request",
            numpadCtaLabel(
                needsAmount: state.needsAmount, confirmedAmountSats: state.confirmedAmountSats
            )
        )
    }

    @MainActor
    func testFreshWalletEntryFetchesTheLiveFloorOnce() async {
        let port = freshWalletPort()
        _ = await startController(port)
        // R6: the one live-floor fetch per visit fired (no capacity).
        XCTAssertEqual(1, port.floorFetches)
    }

    @MainActor
    func testCapacityCoveredEntrySkipsTheFloorFetchAndShowsTheStandardQr() async {
        let port = FakeReceivePort()
        port.inboundMsat = 100_000_000 // covers the static floor
        port.bundleFor = { _ in makeBundle(offer: testReceiveOffer) }
        let c = await startController(port)

        let state = c.state
        XCTAssertFalse(state.needsAmount)
        XCTAssertFalse(state.editingAmount)
        XCTAssertEqual(.display(invoicePath: .standard), state.step)
        XCTAssertEqual(0, port.floorFetches)
        XCTAssertTrue(
            showBolt12Page(
                offerExists: state.offerQrValue != nil, needsAmount: state.needsAmount
            )
        )
    }

    @MainActor
    func testJitConfirmRunsQuoteIntoReview() async {
        let port = freshWalletPort()
        port.quoteFor = { makeQuote(amountMsat: $0, openingFeeMsat: 2_500_000) }
        let c = await startController(port)

        enterAmount(c, "10000")
        // The quoting skeleton presents in the same update as the commit.
        XCTAssertEqual(.quoting, c.state.step)
        await waitUntil {
            if case .jitReview = c.state.step { return true }
            return false
        }

        guard case let .jitReview(review) = c.state.step else {
            return XCTFail("expected the JIT review step, got \(c.state.step)")
        }
        XCTAssertEqual(10_000, review.amountSats)
        XCTAssertEqual(2_500, review.setupFeeSats)
        XCTAssertEqual(7_500, review.youReceiveSats)
        XCTAssertFalse(review.quoteUpdated)
    }

    @MainActor
    func testBelowFloorConfirmIsBlockedBeforeAnyQuote() async {
        // AE4: no quote (and certainly no buy) is issued below the floor.
        var quoted = false
        let port = freshWalletPort()
        port.quoteFor = { amountMsat in
            quoted = true
            return makeQuote(amountMsat: amountMsat)
        }
        let c = await startController(port)

        enterAmount(c, "500")
        // Give any (wrongly) launched work a chance to run before asserting.
        await waitUntil(timeout: 0.2) { false }

        XCTAssertTrue(c.state.editingAmount)
        XCTAssertEqual("", c.state.confirmedDigits)
        XCTAssertFalse(quoted)
    }

    @MainActor
    func testBuySuccessRendersTheJitQrWithFeeAndExpiryThenFlips() async {
        let port = freshWalletPort()
        port.acceptFor = { _, _ in
            makeJitInvoice(openingFeeMsat: 2_500_000, expiresAtUnix: 600)
        }
        let gate = GatedSleep()
        let c = await startController(port, sleepMs: { await gate.sleep($0) })

        enterAmount(c, "10000")
        await waitUntil {
            if case .jitReview = c.state.step { return true }
            return false
        }
        c.generateInvoice()
        await waitUntil { c.state.step == .display(invoicePath: .jit) }

        let state = c.state
        XCTAssertEqual(.display(invoicePath: .jit), state.step)
        XCTAssertEqual(2_500, state.openingFeeSats)
        XCTAssertEqual(600, state.expiresAtUnix)
        XCTAssertTrue(state.qrValue.contains("LIGHTNING="))
        XCTAssertEqual(
            "Setup fee: ₿2,500",
            qrCaption(page: .unified, invoicePath: .jit, openingFeeSats: state.openingFeeSats)
        )

        // The expiry flip fires when the clamped validity passes (now = 0,
        // expiry = 600s → the timer armed for 600,000 ms).
        await waitUntil { gate.requestedMs != nil }
        XCTAssertEqual(600_000, gate.requestedMs)
        gate.fire()
        await waitUntil { c.state.step == .jitExpired }
        XCTAssertEqual(.jitExpired, c.state.step)
    }

    @MainActor
    func testStaleBuyReQuotesTheSameLsp() async {
        let port = freshWalletPort()
        var buys = 0
        port.acceptFor = { _, _ in
            buys += 1
            throw kotlinError(WalletException.JitReQuoteRequired())
        }
        let c = await startController(port)

        enterAmount(c, "10000")
        await waitUntil {
            if case .jitReview = c.state.step { return true }
            return false
        }
        c.generateInvoice()
        await waitUntil {
            if case let .jitReview(review) = c.state.step { return review.quoteUpdated }
            return false
        }

        // Back on Review with a fresh quote, flagged as updated.
        guard case let .jitReview(review) = c.state.step else {
            return XCTFail("expected the JIT review step, got \(c.state.step)")
        }
        XCTAssertTrue(review.quoteUpdated)
        XCTAssertEqual(1, buys)
    }

    @MainActor
    func testOtherBuyFailuresLandOnTheErrorScreen() async {
        let port = freshWalletPort()
        port.acceptFor = { _, _ in
            throw kotlinError(WalletException.Lsps2(reason: "lsps2.buy failed: boom"))
        }
        let c = await startController(port)

        enterAmount(c, "10000")
        await waitUntil {
            if case .jitReview = c.state.step { return true }
            return false
        }
        c.generateInvoice()
        await waitUntil { c.state.step == .jitError }

        XCTAssertEqual(.jitError, c.state.step)
    }

    @MainActor
    func testBelowMinimumQuoteFailureShowsTheDisabledReviewAndRaisesTheGate() async {
        let port = freshWalletPort()
        port.quoteFor = { _ in
            throw kotlinError(
                WalletException.Lsps2(
                    reason: "LSPS2 request failed: amount 4000000msat is below the LSP "
                        + "minimum payment size of 5000000msat"
                )
            )
        }
        port.floorSats = 5_500 // the refreshed headroom-adjusted floor
        let c = await startController(port)

        enterAmount(c, "4000")
        await waitUntil {
            if case .jitBelowMinimum = c.state.step { return true }
            return false
        }

        let state = c.state
        XCTAssertEqual(
            .jitBelowMinimum(amountSats: 4_000, displayMinSats: 5_500), state.step
        )
        // The numpad gate now blocks the same amount up front.
        XCTAssertEqual(5_500, state.floorSats)
    }

    @MainActor
    func testNonSizeQuoteFailureFallsBackToTheOnchainQr() async {
        let port = freshWalletPort()
        port.quoteFor = { _ in
            throw kotlinError(WalletException.Lsps2(reason: "lsps2.get_info failed: boom"))
        }
        let c = await startController(port)

        enterAmount(c, "10000")
        await waitUntil { c.state.step == .display(invoicePath: .none) }

        XCTAssertEqual(.display(invoicePath: .none), c.state.step)
    }

    @MainActor
    func testMatchingPaymentSettlesTheVisit() async {
        let port = FakeReceivePort()
        port.inboundMsat = 100_000_000
        port.bundleFor = { _ in makeBundle(paymentHash: "feed") }
        let c = await startController(port)

        port.emit(
            .paymentReceived(paymentHash: "feed", amountMsat: 10_000_000, skimmedFeeMsat: nil)
        )
        await waitUntil { c.state.step == .received(amountSats: 10_000) }

        XCTAssertEqual(.received(amountSats: 10_000), c.state.step)
    }

    @MainActor
    func testMismatchedPaymentDoesNotSettle() async {
        let port = FakeReceivePort()
        port.inboundMsat = 100_000_000
        port.bundleFor = { _ in makeBundle(paymentHash: "feed") }
        let c = await startController(port)

        port.emit(
            .paymentReceived(paymentHash: "beef", amountMsat: 10_000_000, skimmedFeeMsat: nil)
        )
        // Give the watcher a chance to (wrongly) settle before asserting.
        await waitUntil(timeout: 0.2) { false }

        XCTAssertEqual(.display(invoicePath: .standard), c.state.step)
    }

    @MainActor
    func testBackFromReviewRestoresTheNumpadWithTheAmountPreserved() async {
        let port = freshWalletPort()
        let c = await startController(port)

        enterAmount(c, "10000")
        await waitUntil {
            if case .jitReview = c.state.step { return true }
            return false
        }
        c.backFromReview()
        await waitUntil { !c.state.loading }

        let state = c.state
        XCTAssertTrue(state.editingAmount)
        XCTAssertEqual("10000", state.amountDigits)
        XCTAssertEqual("", state.confirmedDigits)
    }

    @MainActor
    func testRemoveAmountStaysOnTheNumpadWhenAmountIsMandatory() async {
        let port = freshWalletPort()
        let c = await startController(port)

        enterAmount(c, "10000")
        await waitUntil {
            if case .jitReview = c.state.step { return true }
            return false
        }
        c.removeAmount()
        await waitUntil { !c.state.loading }

        let state = c.state
        XCTAssertTrue(state.editingAmount)
        XCTAssertEqual("", state.amountDigits)
        XCTAssertEqual("", state.confirmedDigits)
    }

    @MainActor
    func testEntryFailureShowsTheLoadError() async {
        let port = FakeReceivePort()
        port.bundleFor = { _ in throw kotlinError(WalletException.NotRunning()) }
        let c = await startController(port)

        let state = c.state
        XCTAssertFalse(state.loading)
        XCTAssertNil(state.address)
        XCTAssertNotNil(state.loadError)
    }
}
