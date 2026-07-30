import Foundation
import Shared

// MARK: - Port

/// The receive flow's window onto the wallet (U21, R14): every call is a thin
/// passthrough to the core FFI — the capacity decision, floor computation,
/// quote/buy protocol, and the invoice-expiry clamp all happen in Rust.
/// `WalletModel` implements this; tests can fake it. Mirrors Android's
/// `ReceivePort`.
@MainActor
protocol ReceivePort: AnyObject {
    func receiveBundle(amountMsat: UInt64?) async throws -> ReceiveBundle
    func jitQuote(amountMsat: UInt64) async throws -> JitQuote
    func jitAccept(quoteToken: UInt64, amountMsat: UInt64) async throws -> JitInvoice

    /// R6: the live JIT floor; `refresh = true` is the one get_info per visit.
    func minReceiveSats(refresh: Bool) async throws -> UInt64

    /// Sum of usable inbound capacity, for the typing-time JIT gate (AE4).
    func usableInboundMsat() async throws -> UInt64

    /// The core's `build_bip321_uri` (copy form) — re-composes around a JIT invoice.
    func buildUnifiedUri(address: String, amountSats: UInt64?, invoice: String?) async throws -> String

    /// Mints the persistent BOLT12 offer, or serves the persisted one (R6).
    /// `nil` when the node is stopped or every attempt failed. Blinded paths
    /// need the synced graph, so the core retries on its 3/6/12/24/48 s
    /// schedule — this can block for ~93 s and never belongs on the entry path.
    func getOrCreateOffer() async throws -> String?

    /// The core's `build_bolt12_page_uri` — the offer page's copy form.
    func bolt12Uri(offer: String) async throws -> String

    /// The core's live event rebroadcast (`paymentReceived` settles the
    /// visit). Each access is a fresh subscription registered synchronously
    /// at creation (Android's `walletEvents` shared-flow twin).
    var walletEvents: AsyncStream<WalletEvent> { get }
}

// MARK: - UI state

/// Everything the Receive screen renders (U21). `step` is the PWA's machine;
/// the flat fields mirror the PWA's `useState` cluster (`Receive.tsx:72-93`)
/// — field-for-field with Android's `ReceiveUiState`.
struct ReceiveUiState: Equatable {
    var loading = true
    /// Fatal entry failure (PWA's onchain-error screen, `Receive.tsx:592-602`).
    var loadError: String?
    var address: String?
    /// Copy/share form (address uppercased, rest untouched).
    var bip321Uri = ""
    /// QR form (whole URI uppercased for alphanumeric mode, `Receive.tsx:640`).
    var qrValue = ""
    var offer: String?
    var offerQrValue: String?
    /// PWA `Receive.tsx:290`: amounted standard invoice failed; QR still renders.
    var invoiceError: String?
    /// The displayed invoice's hash — what `applyPaymentReceived` awaits.
    var paymentHash: String?
    /// JIT only: the agreed fee for the under-QR caption (`Receive.tsx:999`).
    var openingFeeSats: UInt64?
    /// JIT only: UNIX seconds when the displayed invoice stops being payable.
    var expiresAtUnix: UInt64?
    /// No usable channels → JIT required → amount required (`Receive.tsx:112-116`).
    var needsAmount = true
    /// The session numpad floor (core-computed: live when fetched, else static).
    var floorSats: UInt64 = 3_000
    /// Usable inbound capacity snapshot for the typing-time gate.
    var usableInboundMsat: UInt64 = 0
    var step: ReceiveStep = .display(invoicePath: .none)
    var editingAmount = false
    var amountDigits = ""
    var confirmedDigits = ""

    var editingAmountSats: UInt64 { UInt64(amountDigits) ?? 0 }
    var confirmedAmountSats: UInt64 { UInt64(confirmedDigits) ?? 0 }
}

// MARK: - Controller

/// Drives `ReceiveStep` through the core (U21): executes the pure layer's
/// decisions, owns the tasks and the expiry timer, and routes typed failures
/// through `classifyQuoteFailure`/`classifyBuyFailure`. One instance per
/// Receive visit — Android's `ReceiveController` ported intent-for-intent
/// onto Swift concurrency.
@MainActor
final class ReceiveController: ObservableObject {
    @Published private(set) var state = ReceiveUiState()

    private let port: any ReceivePort
    private let nowUnixSecs: () -> Int64
    /// Injectable timer sleep (milliseconds) so tests control the expiry flip.
    private let sleepMs: (UInt64) async throws -> Void

    private var watcherTask: Task<Void, Never>?
    private var requestTask: Task<Void, Never>?
    private var expiryTask: Task<Void, Never>?
    private var offerTask: Task<Void, Never>?
    private var started = false

    init(
        port: any ReceivePort,
        nowUnixSecs: @escaping () -> Int64 = { Int64(Date().timeIntervalSince1970) },
        sleepMs: @escaping (UInt64) async throws -> Void = {
            try await Task.sleep(nanoseconds: $0 * 1_000_000)
        }
    ) {
        self.port = port
        self.nowUnixSecs = nowUnixSecs
        self.sleepMs = sleepMs
    }

    deinit {
        watcherTask?.cancel()
        requestTask?.cancel()
        expiryTask?.cancel()
        offerTask?.cancel()
    }

    /// Screen entry: floor fetch + the amountless default bundle + settlement watch.
    func start() {
        guard !started else { return }
        started = true

        // Success watcher (PWA Receive.tsx:332-343): subscribe for the whole
        // visit; the first paymentReceived matching our displayed invoice
        // settles the screen from any step. The stream's sink registers
        // synchronously here, before any entry FFI work dispatches.
        let events = port.walletEvents
        watcherTask = Task { [weak self] in
            for await event in events {
                guard let self else { return }
                if let settled = applyPaymentReceived(
                    awaitedPaymentHash: self.state.paymentHash, event: event
                ) {
                    self.state.step = settled
                }
            }
        }

        requestTask = Task { [weak self] in
            guard let self else { return }
            do {
                let inbound = try await self.port.usableInboundMsat()
                // R6: one live-floor get_info per visit, and only when it can
                // matter — the gate binds only if usable inbound capacity is
                // itself below the static floor (PWA Receive.tsx:158-176).
                // Failure degrades to the core's static fallback silently.
                if inbound < self.state.floorSats * 1_000 {
                    _ = try? await self.port.minReceiveSats(refresh: true)
                }
                let bundle = try await self.port.receiveBundle(amountMsat: nil)
                guard !Task.isCancelled else { return }
                var next = applyBundle(self.state, bundle)
                next.loading = false
                next.usableInboundMsat = inbound
                next.needsAmount = bundle.needsJit
                // Start on the numpad when amount is required
                // (PWA Receive.tsx:137-143).
                next.editingAmount = bundle.needsJit
                next.step = .display(
                    invoicePath: bundle.bolt11 != nil ? .standard : InvoicePath.none
                )
                self.state = next
                // An amountless `needsJit` IS the core's `has_usable_channel`
                // (rust/src/receive.rs `needs_jit`), so this is exactly the
                // offer gate: mint only when the offer page could render.
                if !bundle.needsJit, bundle.offer == nil { self.mintOffer() }
            } catch {
                guard !Task.isCancelled else { return }
                self.state.loading = false
                self.state.loadError = walletErrorMessage(error)
            }
        }
    }

    func onNumpadKey(_ key: NumpadInput) {
        state.amountDigits = NumpadReducer.reduce(
            state.amountDigits, key, maxDigits: receiveMaxDigits
        )
    }

    /// Numpad Next (PWA `handleConfirmAmount`, `Receive.tsx:425-439`).
    func confirmAmount() {
        let s = state
        let decision = confirmAmountDecision(
            amountSats: s.editingAmountSats,
            usableInboundMsat: s.usableInboundMsat,
            floorSats: s.floorSats
        )
        guard case let .request(amountSats, presentQuoting) = decision else { return }
        var next = state
        next.confirmedDigits = next.amountDigits
        next.editingAmount = false
        // Flip to the quoting skeleton in the same update as the commit so no
        // stale QR frame renders (PWA Receive.tsx:430-435).
        if presentQuoting { next.step = .quoting }
        state = next
        requestBundle(amountSats: amountSats)
    }

    /// PWA `handleCancelAmount` (`Receive.tsx:441-445`).
    func cancelAmount() {
        state.amountDigits = state.confirmedDigits
        state.editingAmount = false
    }

    /// PWA `handleEditAmount` (`Receive.tsx:459-462`).
    func editAmount() {
        state.amountDigits = state.confirmedDigits
        state.editingAmount = true
    }

    /// PWA `handleRemoveAmount` (`Receive.tsx:447-457`): back to the amountless QR.
    func removeAmount() {
        var next = state
        next.amountDigits = ""
        next.confirmedDigits = ""
        // Stay on the numpad when the amount is mandatory.
        next.editingAmount = next.needsAmount
        state = next
        requestBundle(amountSats: 0)
    }

    /// Review/expired/error Back (PWA `handleReviewBack`, `Receive.tsx:560-568`):
    /// abandon the quote, restore the numpad with the amount preserved, and
    /// regenerate the amountless default behind it.
    func backFromReview() {
        var next = state
        next.amountDigits = next.confirmedDigits
        next.confirmedDigits = ""
        next.step = .display(invoicePath: .none)
        next.editingAmount = true
        state = next
        requestBundle(amountSats: 0)
    }

    /// Expired/error retry (PWA `handleErrorRetry`): re-run the flow at the amount.
    func retryRequest() {
        let amountSats = state.confirmedAmountSats
        if amountSats == 0 {
            backFromReview()
            return
        }
        state.step = .quoting
        requestBundle(amountSats: amountSats)
    }

    /// Review CTA (PWA `handleGenerateInvoice`, `Receive.tsx:503-558`): Phase B.
    func generateInvoice() {
        guard case let .jitReview(review) = state.step else { return }
        requestTask?.cancel()
        requestTask = Task { [weak self] in
            guard let self else { return }
            self.state.step = .buying
            do {
                let invoice = try await self.port.jitAccept(
                    quoteToken: review.quote.quoteToken,
                    amountMsat: review.quote.amountMsat
                )
                let address = self.state.address ?? ""
                let uri = try await self.port.buildUnifiedUri(
                    address: address, amountSats: review.amountSats, invoice: invoice.bolt11
                )
                guard !Task.isCancelled else { return }
                var next = self.state
                next.bip321Uri = uri
                next.qrValue = uri.uppercased()
                next.paymentHash = invoice.paymentHash
                // PWA fee display ceils: (openingFeeMsat + 999n) / 1000n.
                next.openingFeeSats = UInt64(
                    FormatKt.msatToSatCeil(msat: Int64(bitPattern: invoice.openingFeeMsat))
                )
                next.expiresAtUnix = invoice.expiresAtUnix
                next.invoiceError = nil
                next.step = .display(invoicePath: .jit)
                self.state = next
                self.scheduleExpiry(invoice.expiresAtUnix)
            } catch {
                guard !Task.isCancelled else { return }
                switch classifyBuyFailure(error) {
                // Stale quote, raised BEFORE any buy — re-quote the same LSP
                // set (PWA Receive.tsx:534-537).
                case .reQuote:
                    await self.runQuote(amountSats: review.amountSats, reQuote: true)
                case .error:
                    self.state.step = .jitError
                }
            }
        }
    }

    // ------------------------------------------------------------------

    /// The PWA's flow-driver effect (`Receive.tsx:194-298`): fetch a bundle
    /// for the confirmed amount; the core's `needs_jit` routes to the
    /// standard QR or into Phase A.
    private func requestBundle(amountSats: UInt64) {
        requestTask?.cancel()
        requestTask = Task { [weak self] in
            guard let self else { return }
            let amountMsat: UInt64? = amountSats > 0 ? amountSats * 1_000 : nil
            let bundle: ReceiveBundle
            do {
                bundle = try await self.port.receiveBundle(amountMsat: amountMsat)
            } catch {
                guard !Task.isCancelled else { return }
                // PWA Receive.tsx:285-293: the on-chain QR keeps rendering;
                // an amounted failure surfaces the invoice error copy.
                var next = self.state
                next.invoiceError = amountSats > 0 ? "Failed to create Lightning invoice" : nil
                next.paymentHash = nil
                next.openingFeeSats = nil
                next.expiresAtUnix = nil
                next.step = .display(invoicePath: .none)
                self.state = next
                return
            }
            guard !Task.isCancelled else { return }
            if bundle.needsJit, amountMsat != nil {
                var next = applyBundle(self.state, bundle)
                next.step = .quoting
                self.state = next
                await self.runQuote(amountSats: amountSats, reQuote: false)
            } else {
                var next = applyBundle(self.state, bundle)
                next.step = .display(
                    invoicePath: bundle.bolt11 != nil ? .standard : InvoicePath.none
                )
                self.state = next
            }
        }
    }

    /// JIT Phase A (PWA `Receive.tsx:220-274` + `reQuote`, `Receive.tsx:473-501`).
    private func runQuote(amountSats: UInt64, reQuote: Bool) async {
        do {
            let quote = try await port.jitQuote(amountMsat: amountSats * 1_000)
            guard !Task.isCancelled else { return }
            state.step = .jitReview(
                ReceiveJitReview(amountSats: amountSats, quote: quote, quoteUpdated: reQuote)
            )
        } catch {
            guard !Task.isCancelled else { return }
            switch classifyQuoteFailure(error) {
            case .belowMinimum:
                // Sync the numpad gate to the freshest observed menu and
                // render the below-minimum review (PWA Receive.tsx:249-265).
                // The suggested minimum is the core's headroom-adjusted floor
                // (its computeMinReceiveSats), never the raw menu min.
                let refreshed = (try? await port.minReceiveSats(refresh: true)) ?? 0
                guard !Task.isCancelled else { return }
                let displayMin = refreshed > 0 ? refreshed : state.floorSats
                state.floorSats = displayMin
                state.step = .jitBelowMinimum(amountSats: amountSats, displayMinSats: displayMin)

            case .other:
                if reQuote {
                    // Re-quote failure → jit-error (PWA Receive.tsx:494-498).
                    state.step = .jitError
                } else {
                    // Phase A failure → on-chain-only QR (PWA Receive.tsx:266-268).
                    state.step = .display(invoicePath: .none)
                }
            }
        }
    }

    /// The PWA's `loadOrCreateOffer` (`context.tsx:1655-1663`): the offer page
    /// needs an offer only `ReceivePort.getOrCreateOffer` can mint, and minting
    /// blocks through the core's retry schedule — so it runs BESIDE the screen
    /// on its own task, never on the entry path. Only the offer fields are
    /// folded in: by the time it lands the visit may be mid-JIT-flow, and the
    /// rest of the state belongs to that flow. A failure is silent — the pager
    /// simply stays at one page (the PWA swallows it the same way).
    private func mintOffer() {
        offerTask?.cancel()
        offerTask = Task { [weak self] in
            guard let self else { return }
            // Offer creation NEVER degrades receive (core contract, R6).
            guard let offer = try? await self.port.getOrCreateOffer(),
                  let uri = try? await self.port.bolt12Uri(offer: offer),
                  !Task.isCancelled
            else { return }
            self.state.offer = offer
            self.state.offerQrValue = uri.uppercased()
        }
    }

    /// The expiry flip (PWA `Receive.tsx:319-330`), guarded by `applyExpiryFlip`.
    private func scheduleExpiry(_ expiresAtUnix: UInt64) {
        expiryTask?.cancel()
        let delayMs = UInt64(
            countdownSecondsLeft(expiresAtUnix: expiresAtUnix, nowUnixSecs: nowUnixSecs())
        ) * 1_000
        expiryTask = Task { [weak self] in
            guard let self else { return }
            do {
                try await self.sleepMs(delayMs)
            } catch {
                return
            }
            guard !Task.isCancelled else { return }
            self.state.step = applyExpiryFlip(self.state.step)
        }
    }

    /// Fold a fresh core bundle into the state (URIs, offer, hash, floor).
    private func applyBundle(_ s: ReceiveUiState, _ bundle: ReceiveBundle) -> ReceiveUiState {
        var next = s
        next.address = bundle.address
        next.bip321Uri = bundle.bip321Uri
        next.qrValue = bundle.qrValue
        next.offer = bundle.offer
        next.offerQrValue = bundle.offerQrValue
        next.invoiceError = bundle.invoiceError
        next.paymentHash = bundle.paymentHash
        next.openingFeeSats = nil
        next.expiresAtUnix = nil
        next.floorSats = bundle.minReceiveSats
        return next
    }
}
