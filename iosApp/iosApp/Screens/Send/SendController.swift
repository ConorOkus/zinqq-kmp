import Foundation
import Shared

// MARK: - Port

/// The send flow's window onto the wallet (U20, R14): every call is a thin
/// passthrough to the core FFI — classification, resolution, fee estimates,
/// and dispatch all happen in Rust. `WalletModel` implements this; tests can
/// fake it. Mirrors Android's `SendPort`.
@MainActor
protocol SendPort: AnyObject {
    func classify(_ input: String) async throws -> ClassifiedView
    func resolve(_ input: String) async throws -> ResolvedView
    func fetchLnurlInvoice(_ lnurl: LnurlPayView, amountMsat: UInt64) async throws -> ClassifiedView
    func sendBolt11(_ bolt11: String, amountMsat: UInt64?) async throws
    func payOffer(_ offer: String, amountMsat: UInt64?) async throws
    func estimateOnchainFee(address: String, amountSats: UInt64) async throws -> FeeEstimate
    func estimateMaxSendable(address: String) async throws -> MaxSendEstimate
    func sendOnchain(
        address: String,
        amountSats: UInt64,
        expectedAmountSats: UInt64,
        expectedFeeSats: UInt64
    ) async throws -> String
    func sendOnchainMax(
        address: String,
        expectedAmountSats: UInt64,
        expectedFeeSats: UInt64
    ) async throws -> String

    /// The core's live event rebroadcast (payment outcomes arrive here, F1).
    /// Each access is a fresh subscription: the sink is registered
    /// synchronously at creation, so subscribing BEFORE dispatch cannot miss
    /// an instant outcome (Android subscribes `walletEvents` the same way).
    var walletEvents: AsyncStream<WalletEvent> { get }

    /// Snapshot balances for the PWA's UI gates (capacity / available).
    func lightningCapacityMsat() -> UInt64
    func onchainBalanceSats() -> UInt64
}

/// Race the first payment outcome on `events` against the 5-minute cap;
/// `nil` means the cap fired first (Android's `withTimeoutOrNull` +
/// `first { isPaymentOutcome(it) }`).
func firstPaymentOutcome(
    in events: AsyncStream<WalletEvent>,
    timeoutMs: UInt64
) async -> WalletEvent? {
    await withTaskGroup(of: WalletEvent?.self) { group in
        group.addTask {
            for await event in events where isPaymentOutcome(event) { return event }
            return nil
        }
        group.addTask {
            try? await Task.sleep(nanoseconds: timeoutMs * 1_000_000)
            return nil
        }
        let first = await group.next() ?? nil
        group.cancelAll()
        return first
    }
}

// MARK: - Controller

/// Drives `SendStep` through the core (U20): executes the pure layer's
/// `SendDecision`s, owns the tasks, and maps typed failures through
/// `walletErrorMessage`. One instance per Send visit — Android's
/// `SendController` ported intent-for-intent onto Swift concurrency.
@MainActor
final class SendController: ObservableObject {
    @Published private(set) var step: SendStep = .input()

    private let port: any SendPort
    private let outcomeTimeoutMs: UInt64
    private var job: Task<Void, Never>?

    init(port: any SendPort, outcomeTimeoutMs: UInt64 = sendOutcomeTimeoutMs) {
        self.port = port
        self.outcomeTimeoutMs = outcomeTimeoutMs
    }

    /// Recipient-screen Continue / paste / scanned input (same path, R13).
    func submitInput(_ raw: String) {
        let trimmed = raw.trimmingCharacters(in: .whitespacesAndNewlines)
        if trimmed.isEmpty {
            step = .input(error: "Enter a payment request or address")
            return
        }
        if trimmed.count > sendInputMaxLength {
            step = .input(error: "Scanned input is too long")
            return
        }
        job?.cancel()
        job = Task { [weak self] in
            guard let self else { return }
            let view: ClassifiedView
            do {
                view = try await self.port.classify(trimmed)
            } catch {
                guard !Task.isCancelled else { return }
                self.step = .input(error: walletErrorMessage(error))
                return
            }
            guard !Task.isCancelled else { return }
            await self.execute(
                routeInput(
                    raw: trimmed,
                    view: view,
                    lnurl: nil,
                    lnCapacityMsat: self.port.lightningCapacityMsat(),
                    onchainBalanceSats: self.port.onchainBalanceSats()
                )
            )
        }
    }

    /// Abort an in-flight BIP353/LNURL resolution (PWA `Send.tsx:1180-1183`).
    func abortResolve() {
        job?.cancel()
        job = nil
        step = .input()
    }

    func onNumpadKey(_ key: NumpadInput) {
        guard case let .amount(amount) = step else { return }
        step = .amount(reduceAmountKey(amount, key: key))
    }

    /// On-chain Max (PWA `handleOnchainSendAll`): the exact prefill comes
    /// from the core's drain estimate — the shell never does reserve math.
    func setOnchainSendMax() {
        guard case let .amount(amount) = step, let address = amount.target.address else { return }
        job?.cancel()
        job = Task { [weak self] in
            guard let self else { return }
            do {
                let estimate = try await self.port.estimateMaxSendable(address: address)
                guard !Task.isCancelled, case var .amount(current) = self.step else { return }
                current.digits = String(estimate.amountSats)
                current.isSendMax = true
                current.error = nil
                self.step = .amount(current)
            } catch {
                guard !Task.isCancelled, case var .amount(current) = self.step else { return }
                current.error = walletErrorMessage(error)
                self.step = .amount(current)
            }
        }
    }

    /// Lightning "₿X available" prefill (PWA `handleApproxSendMax`).
    func setLightningAvailable() {
        guard case var .amount(amount) = step else { return }
        let total = unifiedTotalSats(
            onchainBalanceSats: port.onchainBalanceSats(),
            lightningMsat: port.lightningCapacityMsat()
        )
        let prefill = lnAvailablePrefillSats(unifiedTotalSats: total, maxSats: amount.maxSats)
        if prefill == 0 { return }
        amount.digits = String(prefill)
        amount.error = nil
        step = .amount(amount)
    }

    func submitAmountStep() {
        guard case let .amount(amount) = step, amount.amountSats > 0 else { return }
        job?.cancel()
        job = Task { [weak self] in
            guard let self else { return }
            await self.execute(
                submitAmount(
                    amount,
                    lnCapacityMsat: self.port.lightningCapacityMsat(),
                    onchainBalanceSats: self.port.onchainBalanceSats()
                )
            )
        }
    }

    /// Review back (PWA `handleReviewBack`): amount step if it existed, else input.
    func backFromReview() {
        var back: SendAmountStep?
        switch step {
        case let .reviewLightning(review): back = review.returnTo
        case let .reviewOnchain(review): back = review.returnTo
        default: back = nil
        }
        if var back {
            back.error = nil
            back.fetchingInvoice = false
            step = .amount(back)
        } else {
            step = .input()
        }
    }

    /// Amount-step header back → recipient screen.
    func backToInput() {
        step = .input()
    }

    /// Result-screen Try Again (PWA `Send.tsx:930-941`).
    func retry(_ step: SendStep) {
        self.step = step
    }

    /// Confirm Send on the Lightning review (PWA `handleLnConfirm`).
    func confirmLightning() {
        guard case let .reviewLightning(review) = step else { return }
        job?.cancel()
        job = Task { [weak self] in
            guard let self else { return }
            self.step = .dispatching(amountMsat: review.amountMsat)
            // Subscribe BEFORE dispatch so an instant outcome cannot be
            // missed (the stream's sink registers synchronously here).
            let events = self.port.walletEvents
            let timeoutMs = self.outcomeTimeoutMs
            let outcome = Task { await firstPaymentOutcome(in: events, timeoutMs: timeoutMs) }
            do {
                // The amount override is REQUIRED for amountless requests and
                // REJECTED otherwise (core U6 matrix) — key off the embedded
                // amount exactly like the PWA (Send.tsx:748-758).
                let override: UInt64? =
                    review.target.amountMsat == nil ? review.amountMsat : nil
                if review.target.kind == .bolt12 {
                    guard let offer = review.target.offer else {
                        throw SendPortError.walletUnavailable
                    }
                    try await self.port.payOffer(offer, amountMsat: override)
                } else {
                    guard let bolt11 = review.target.bolt11 else {
                        throw SendPortError.walletUnavailable
                    }
                    try await self.port.sendBolt11(bolt11, amountMsat: override)
                }
            } catch {
                outcome.cancel()
                guard !Task.isCancelled else { return }
                self.step = .failure(message: walletErrorMessage(error), retry: nil)
                return
            }
            let event = await outcome.value
            guard !Task.isCancelled else { return }
            if let event, let settled = applyOutcome(amountMsat: review.amountMsat, event: event) {
                self.step = settled
            } else {
                self.step = outcomeTimedOut(amountMsat: review.amountMsat)
            }
        }
    }

    /// Confirm Send on the on-chain review (PWA `handleOcConfirm`, R5/R7).
    func confirmOnchain() {
        guard case var .reviewOnchain(review) = step, !review.broadcasting else { return }
        job?.cancel()
        job = Task { [weak self] in
            guard let self else { return }
            review.broadcasting = true
            self.step = .reviewOnchain(review)
            do {
                let txid: String
                if review.isSendMax {
                    txid = try await self.port.sendOnchainMax(
                        address: review.address,
                        expectedAmountSats: review.amountSats,
                        expectedFeeSats: review.feeSats
                    )
                } else {
                    txid = try await self.port.sendOnchain(
                        address: review.address,
                        amountSats: review.amountSats,
                        expectedAmountSats: review.amountSats,
                        expectedFeeSats: review.feeSats
                    )
                }
                guard !Task.isCancelled else { return }
                self.step = .success(amountSats: review.amountSats, txid: txid)
            } catch {
                guard !Task.isCancelled else { return }
                if kotlinWalletException(error) is WalletException.OnchainAmountChanged {
                    // R5 drift guard: nothing was signed or broadcast —
                    // re-run the estimate and re-render the review with the
                    // "Amounts were updated" banner (PWA Send.tsx:678-716).
                    await self.refreshReviewAfterDrift(review)
                } else {
                    self.routeConfirmFailure(review, error)
                }
            }
        }
    }

    private func refreshReviewAfterDrift(_ review: SendOnchainReview) async {
        do {
            if review.isSendMax {
                let fresh = try await port.estimateMaxSendable(address: review.address)
                guard !Task.isCancelled else { return }
                step = .reviewOnchain(refreshedMaxReview(review, fresh: fresh))
            } else {
                let fresh = try await port.estimateOnchainFee(
                    address: review.address,
                    amountSats: review.amountSats
                )
                guard !Task.isCancelled else { return }
                step = .reviewOnchain(refreshedExactReview(review, fresh: fresh))
            }
        } catch {
            guard !Task.isCancelled else { return }
            routeConfirmFailure(review, error)
        }
    }

    private func routeConfirmFailure(_ review: SendOnchainReview, _ error: Error) {
        let message = walletErrorMessage(error)
        if isAmountStepGuardError(error) {
            // PWA returnToAmountWithError: back to the amount step (or the
            // recipient screen) with the friendly inline message.
            if var back = review.returnTo {
                back.error = message
                back.fetchingInvoice = false
                step = .amount(back)
            } else {
                step = .input(error: message)
            }
        } else {
            var retry = review
            retry.broadcasting = false
            step = .failure(message: message, retry: .reviewOnchain(retry))
        }
    }

    /// Execute a routing decision from the pure layer.
    private func execute(_ decision: SendDecision) async {
        switch decision {
        case let .step(next):
            step = next

        case let .inlineError(message):
            showInlineError(message)

        case let .resolve(input):
            step = .input(resolving: true)
            let resolved: ResolvedView
            do {
                resolved = try await port.resolve(input)
            } catch {
                guard !Task.isCancelled else { return }
                step = .input(error: walletErrorMessage(error))
                return
            }
            guard !Task.isCancelled else { return }
            step = .input()
            await execute(
                routeInput(
                    raw: input,
                    view: resolved.classified,
                    lnurl: resolved.lnurl,
                    lnCapacityMsat: port.lightningCapacityMsat(),
                    onchainBalanceSats: port.onchainBalanceSats()
                )
            )

        case let .fetchLnurlInvoice(lnurl, amountMsat, rawInput, returnTo):
            markBusy(returnTo)
            do {
                let invoice = try await port.fetchLnurlInvoice(lnurl, amountMsat: amountMsat)
                guard !Task.isCancelled else { return }
                step = .reviewLightning(
                    lnurlInvoiceReview(
                        invoice: invoice,
                        requestedMsat: amountMsat,
                        rawInput: rawInput,
                        returnTo: returnTo
                    )
                )
            } catch {
                guard !Task.isCancelled else { return }
                failBusy(returnTo, message: walletErrorMessage(error))
            }

        case let .estimateOnchain(address, amountSats, returnTo):
            markBusy(returnTo)
            do {
                let estimate = try await port.estimateOnchainFee(
                    address: address, amountSats: amountSats
                )
                guard !Task.isCancelled else { return }
                step = .reviewOnchain(
                    onchainReview(
                        address: address,
                        amountSats: amountSats,
                        estimate: estimate,
                        returnTo: returnTo
                    )
                )
            } catch {
                guard !Task.isCancelled else { return }
                failBusy(returnTo, message: walletErrorMessage(error))
            }

        case let .estimateOnchainMax(address, returnTo):
            markBusy(returnTo)
            do {
                let estimate = try await port.estimateMaxSendable(address: address)
                guard !Task.isCancelled else { return }
                step = .reviewOnchain(
                    onchainMaxReview(address: address, estimate: estimate, returnTo: returnTo)
                )
            } catch {
                guard !Task.isCancelled else { return }
                failBusy(returnTo, message: walletErrorMessage(error))
            }
        }
    }

    private func showInlineError(_ message: String) {
        switch step {
        case var .input(current):
            current.error = message
            current.resolving = false
            step = .input(current)
        case var .amount(current):
            current.error = message
            current.fetchingInvoice = false
            step = .amount(current)
        default:
            step = .input(error: message)
        }
    }

    /// Show a spinner on whichever screen initiated a slow core call.
    private func markBusy(_ returnTo: SendAmountStep?) {
        if var returnTo {
            returnTo.fetchingInvoice = true
            returnTo.error = nil
            step = .amount(returnTo)
        } else {
            step = .input(resolving: true)
        }
    }

    private func failBusy(_ returnTo: SendAmountStep?, message: String) {
        if var returnTo {
            returnTo.fetchingInvoice = false
            returnTo.error = message
            step = .amount(returnTo)
        } else {
            step = .input(error: message)
        }
    }
}

/// The port's own failure (no wallet yet): mapped to the PWA's not-running
/// copy by `walletErrorMessage`.
enum SendPortError: Error {
    case walletUnavailable
}
