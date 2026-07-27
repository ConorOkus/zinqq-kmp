import Shared
import XCTest

@testable import iosApp

/// The dispatch's outcome await, end to end over a fake port (F1, the P1 fix).
///
/// The core's 5-minute outcome cap (`sendOutcomeTimeoutMs`) deliberately leaves
/// the capped payment IN FLIGHT, so its `PaymentSuccessful`/`PaymentFailed` can
/// arrive while a LATER send is dispatching. Settling on whichever outcome
/// arrives first therefore let one send inherit another's result — a user told
/// their payment failed when it succeeded, or the reverse. These tests pin the
/// hash filter that closes it, and the deliberate BOLT12 exception.
final class SendOutcomeMatchingTests: XCTestCase {

    // MARK: Fake port

    /// Only the Lightning dispatch path is exercised here; the rest of the
    /// port throws so an unexpected call fails loudly instead of silently
    /// returning a fixture.
    @MainActor
    private final class FakeSendPort: SendPort {
        private(set) var bolt11Sends: [(String, UInt64?)] = []
        private(set) var offerSends: [(String, UInt64?)] = []
        private var continuations: [AsyncStream<WalletEvent>.Continuation] = []

        func sendBolt11(_ bolt11: String, amountMsat: UInt64?) async throws {
            bolt11Sends.append((bolt11, amountMsat))
        }

        func payOffer(_ offer: String, amountMsat: UInt64?) async throws {
            offerSends.append((offer, amountMsat))
        }

        func classify(_ input: String) async throws -> ClassifiedView {
            throw SendPortError.walletUnavailable
        }

        func resolve(_ input: String) async throws -> ResolvedView {
            throw SendPortError.walletUnavailable
        }

        func fetchLnurlInvoice(
            _ lnurl: LnurlPayView,
            amountMsat: UInt64
        ) async throws -> ClassifiedView {
            throw SendPortError.walletUnavailable
        }

        func estimateOnchainFee(address: String, amountSats: UInt64) async throws -> FeeEstimate {
            throw SendPortError.walletUnavailable
        }

        func estimateMaxSendable(address: String) async throws -> MaxSendEstimate {
            throw SendPortError.walletUnavailable
        }

        func sendOnchain(
            address: String,
            amountSats: UInt64,
            expectedAmountSats: UInt64,
            expectedFeeSats: UInt64
        ) async throws -> String {
            throw SendPortError.walletUnavailable
        }

        func sendOnchainMax(
            address: String,
            expectedAmountSats: UInt64,
            expectedFeeSats: UInt64
        ) async throws -> String {
            throw SendPortError.walletUnavailable
        }

        /// Fresh subscription per access, registered synchronously — the same
        /// contract as `WalletModel.walletEvents`.
        var walletEvents: AsyncStream<WalletEvent> {
            AsyncStream { continuation in
                continuations.append(continuation)
            }
        }

        func emit(_ event: WalletEvent) {
            for continuation in continuations { continuation.yield(event) }
        }

        func lightningCapacityMsat() -> UInt64 { 100_000_000 }
        func onchainBalanceSats() -> UInt64 { 50_000 }
    }

    // MARK: Harness

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

    /// A controller parked on the Lightning review for `target`, then
    /// confirmed — i.e. dispatched and awaiting its outcome.
    @MainActor
    private func dispatched(
        _ port: FakeSendPort,
        target: ClassifiedView,
        amountMsat: UInt64 = 21_000
    ) async -> SendController {
        let controller = SendController(port: port, outcomeTimeoutMs: 60_000)
        controller.retry(
            .reviewLightning(
                SendLightningReview(
                    target: target,
                    amountMsat: amountMsat,
                    recipient: "satoshi@zinqq.app"
                )
            )
        )
        controller.confirmLightning()
        await waitUntil {
            if case .dispatching = controller.step { return true }
            return false
        }
        return controller
    }

    @MainActor
    private func bolt11Target() -> ClassifiedView {
        classifiedView(
            kind: .bolt11,
            bolt11: testBolt11,
            paymentHash: ourPaymentHash,
            amountMsat: 21_000
        )
    }

    // MARK: Tests

    /// A foreign outcome must NOT settle the dispatch — the await keeps
    /// waiting, and the matching outcome that follows settles it.
    @MainActor
    func testForeignOutcomeDoesNotSettleTheDispatchAndTheMatchingOneDoes() async {
        let port = FakeSendPort()
        let controller = await dispatched(port, target: bolt11Target())
        XCTAssertEqual(1, port.bolt11Sends.count)

        // A previous payment (abandoned in flight by its 5-minute cap) fails.
        port.emit(.paymentFailed(paymentHash: foreignPaymentHash, reason: "no route"))
        // Give the (wrongly) settling await a chance to land before asserting.
        await waitUntil(timeout: 0.2) { false }
        guard case .dispatching = controller.step else {
            return XCTFail("a foreign payment's outcome settled our dispatch: \(controller.step)")
        }

        // OUR payment then succeeds — that is the one that settles us.
        port.emit(.paymentSuccessful(paymentHash: ourPaymentHash))
        await waitUntil { controller.step == .success(amountSats: 21, txid: nil) }
        XCTAssertEqual(.success(amountSats: 21, txid: nil), controller.step)
    }

    /// The inverse pairing: a foreign SUCCESS cannot report our failing
    /// payment as sent.
    @MainActor
    func testForeignSuccessDoesNotMaskOurFailure() async {
        let port = FakeSendPort()
        let controller = await dispatched(port, target: bolt11Target())

        port.emit(.paymentSuccessful(paymentHash: foreignPaymentHash))
        await waitUntil(timeout: 0.2) { false }
        guard case .dispatching = controller.step else {
            return XCTFail("a foreign success settled our dispatch: \(controller.step)")
        }

        port.emit(.paymentFailed(paymentHash: ourPaymentHash, reason: "no route"))
        await waitUntil { controller.step == .failure(message: "no route", retry: nil) }
        XCTAssertEqual(.failure(message: "no route", retry: nil), controller.step)
    }

    /// BOLT12 keeps first-outcome matching: an offer has no payment hash until
    /// the invoice request produces an invoice, so there is nothing to filter
    /// on and the first outcome after dispatch is taken as ours.
    @MainActor
    func testBolt12DispatchStillSettlesOnTheFirstOutcome() async {
        let port = FakeSendPort()
        let controller = await dispatched(
            port,
            target: classifiedView(kind: .bolt12, offer: testOffer, amountMsat: 21_000)
        )
        XCTAssertEqual(1, port.offerSends.count)

        port.emit(.paymentSuccessful(paymentHash: foreignPaymentHash))
        await waitUntil { controller.step == .success(amountSats: 21, txid: nil) }
        XCTAssertEqual(.success(amountSats: 21, txid: nil), controller.step)
    }

    /// The stream-level filter in isolation: foreign outcomes are skipped and
    /// the await returns only ours.
    func testFirstPaymentOutcomeSkipsForeignHashes() async {
        var sink: AsyncStream<WalletEvent>.Continuation? = nil
        let events = AsyncStream<WalletEvent> { sink = $0 }
        sink?.yield(.syncCompleted)
        sink?.yield(.paymentSuccessful(paymentHash: foreignPaymentHash))
        sink?.yield(.paymentFailed(paymentHash: ourPaymentHash, reason: "no route"))

        let outcome = await firstPaymentOutcome(
            in: events,
            awaitedPaymentHash: ourPaymentHash,
            timeoutMs: 60_000
        )
        guard case let .paymentFailed(paymentHash, reason) = outcome else {
            return XCTFail("expected our payment's failure, got \(String(describing: outcome))")
        }
        XCTAssertEqual(ourPaymentHash, paymentHash)
        XCTAssertEqual("no route", reason)
    }

    /// The cap still fires when only foreign outcomes arrive: the dispatch
    /// times out (neutral terminal state) rather than stealing a result.
    func testOnlyForeignOutcomesLetTheCapFire() async {
        var sink: AsyncStream<WalletEvent>.Continuation? = nil
        let events = AsyncStream<WalletEvent> { sink = $0 }
        sink?.yield(.paymentSuccessful(paymentHash: foreignPaymentHash))

        let outcome = await firstPaymentOutcome(
            in: events,
            awaitedPaymentHash: ourPaymentHash,
            timeoutMs: 50
        )
        XCTAssertNil(outcome)
    }
}
