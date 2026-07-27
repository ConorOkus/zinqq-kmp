import Combine
import Foundation
import Shared
import UIKit

// MARK: - Event adapter

/// Swift-side mirror of the shared sealed `Event` hierarchy so the view/model
/// code never touches framework class names directly.
///
/// SKIE-less Kotlin/Native export shape assumed here: the uniffi-generated
/// sealed class `Event` surfaces as a class hierarchy whose nested subclasses
/// get swift_name "Event.NodeStarted", "Event.InvoiceReady", etc. The exact
/// exported names (nested `Event.NodeStarted` vs flattened `EventNodeStarted`)
/// are confirmed at the first Xcode build — if they differ, adjust only
/// `WalletEvent.from(_:)`.
enum WalletEvent {
    case nodeStarted
    case nodeStopped
    case syncFailed
    case syncCompleted
    case invoiceReady(bolt11: String, expiryUnixSecs: UInt64)
    case paymentReceived(amountMsat: UInt64, skimmedFeeMsat: UInt64?)
    case paymentSuccessful
    case paymentFailed(reason: String)
    case channelPending
    case channelReady
    case lsps2Failed(reason: String)
    /// Another client took over this seed's VSS namespace (KTD-3, plan
    /// System-Wide Impact): the core fenced itself durably and halted.
    case fenced(detail: String)
    /// Sealed in Kotlin, but Swift cannot prove exhaustiveness over the
    /// exported class hierarchy; unknown events are ignored by the reducer.
    case unknown

    static func from(_ event: Event) -> WalletEvent {
        switch event {
        case is Event.NodeStarted:
            return .nodeStarted
        case is Event.NodeStopped:
            return .nodeStopped
        case is Event.SyncFailed:
            return .syncFailed
        case is Event.SyncCompleted:
            return .syncCompleted
        case let e as Event.InvoiceReady:
            return .invoiceReady(bolt11: e.bolt11, expiryUnixSecs: e.expiryUnixSecs)
        case let e as Event.PaymentReceived:
            // skimmed_fee_msat is Option<u64> in Rust, so it arrives as KotlinULong?
            return .paymentReceived(amountMsat: e.amountMsat, skimmedFeeMsat: e.skimmedFeeMsat?.uint64Value)
        case is Event.PaymentSuccessful:
            return .paymentSuccessful
        case let e as Event.PaymentFailed:
            return .paymentFailed(reason: e.reason)
        case is Event.ChannelPending:
            return .channelPending
        case is Event.ChannelReady:
            return .channelReady
        case let e as Event.Lsps2Failed:
            return .lsps2Failed(reason: e.reason)
        case let e as Event.Fenced:
            return .fenced(detail: e.detail)
        default:
            return .unknown
        }
    }
}

// MARK: - Model

/// Owns the shared `Wallet` and its handle-then-ack event loop, reducing
/// events into published UI state. No Lightning logic lives here (R4):
/// strings go in, events come out — the core does all parsing, fees, and
/// reconnect (KTD-10).
@MainActor
final class WalletModel: ObservableObject {
    struct Invoice: Equatable {
        let bolt11: String
        let expiryUnixSecs: UInt64
    }

    @Published private(set) var running = false
    @Published private(set) var balanceMsat: UInt64 = 0
    @Published private(set) var currentInvoice: Invoice?
    @Published private(set) var lastOutcome: String?
    @Published private(set) var syncBanner: String?
    /// True while a receiveJit/send FFI call is in flight; the view disables
    /// the Request Invoice / Pay buttons on it (R8: one coarse flag is fine).
    @Published private(set) var busy = false
    /// Another client took over this seed's VSS namespace (U18; KTD-3, plan
    /// System-Wide Impact): set by `Event.Fenced` or a typed `Fenced` start
    /// failure, never cleared by an event — un-fencing is user-owned (restore
    /// or quit) and the core's durable flag survives restart.
    @Published private(set) var fenced = false
    /// Persisted appearance selection (U18, KTD-11). Read synchronously at
    /// init — before the first frame — so no frame renders in the wrong
    /// theme, parity with the PWA's pre-render `data-theme` application.
    @Published var appearanceMode: AppearanceMode = .loadPersisted() {
        didSet { appearanceMode.persist() }
    }

    private var wallet: Wallet?
    private var eventLoop: Task<Void, Never>?
    private var startRequested = false

    // MARK: Blocking FFI dispatch

    /// Runs a blocking Wallet FFI call off the MainActor — rust/src/api.rs
    /// documents start/receive_jit/send as blocking and stop() as blocking up
    /// to ~20s — mirroring the Android shell's Dispatchers.IO wrapping.
    /// Callers hop back to the MainActor (their own isolation) to publish
    /// state. Caveat (same unverified-bindings pattern as WalletEvent.from):
    /// the generated Wallet type must be confirmed thread-safe for calls off
    /// the main thread at the first Xcode build.
    private static func runBlockingFFI<T>(
        _ body: @escaping () throws -> T
    ) async throws -> T {
        try await Task.detached(priority: .userInitiated) {
            try body()
        }.value
    }

    // MARK: Lifecycle (KTD-10: foreground-only node)

    /// Called on scenePhase .active. Starts the node and (re)starts the event
    /// loop; peer reconnect after a suspend is the core's job.
    func start() {
        guard !startRequested else { return }
        startRequested = true
        Task { [weak self] in
            guard let self else { return }
            do {
                let wallet = try self.ensureWallet()
                // start() is a blocking FFI call — run it off the MainActor.
                try await Self.runBlockingFFI { try wallet.start() }
                // The previous loop (if any) exited on NodeStopped, or is still
                // parked on a stale nextEvent — cancel it and start fresh.
                // Cancellation can only land while awaiting nextEvent, before an
                // event is handled, so no event is lost (unacked events redeliver).
                self.eventLoop?.cancel()
                self.eventLoop = Task { [weak self] in
                    await self?.runEventLoop(wallet)
                }
            } catch {
                self.startRequested = false
                if Self.isFencedError(error) {
                    // The durable fence survives restart (KTD-3): a fenced
                    // wallet refuses to start, so the shell re-raises the
                    // fenced screen even though no Event.Fenced will arrive
                    // on this run (U18).
                    self.fenced = true
                } else {
                    self.lastOutcome = "Start failed: \(error.localizedDescription)"
                }
            }
        }
    }

    /// Called on scenePhase .background. `stop()` pushes the terminal
    /// NodeStopped event, which completes a pending nextEvent and lets the
    /// loop exit cleanly. It can block ~20s while the channel manager
    /// persists, so it runs off the MainActor under a UIApplication background
    /// task assertion — otherwise iOS could suspend the process mid-persist.
    func stop() {
        guard startRequested, let wallet else { return }
        startRequested = false
        var assertion = UIBackgroundTaskIdentifier.invalid
        func endAssertion() {
            guard assertion != .invalid else { return }
            UIApplication.shared.endBackgroundTask(assertion)
            assertion = .invalid
        }
        assertion = UIApplication.shared.beginBackgroundTask(withName: "wallet-stop") {
            // Expiration: iOS reclaims the assertion; nothing to cancel —
            // stop() is not interruptible — just release the token.
            endAssertion()
        }
        Task { [weak self] in
            do {
                try await Self.runBlockingFFI { try wallet.stop() }
            } catch {
                self?.lastOutcome = "Stop failed: \(error.localizedDescription)"
            }
            endAssertion()
        }
    }

    // MARK: Intents

    /// Requests a Megalith JIT invoice; the invoice arrives asynchronously as
    /// InvoiceReady (or Lsps2Failed). Sats→msat is unit scaling only, not fee
    /// math (R4).
    func requestInvoice(amountSats: UInt64) {
        guard let wallet, !busy else { return }
        // Bound before scaling: `* 1_000` on an unchecked UInt64 would trap
        // on absurd amounts. Reject instead of crashing.
        guard amountSats > 0, amountSats <= UInt64.max / 1_000 else {
            lastOutcome = "Amount out of range"
            return
        }
        busy = true
        lastOutcome = nil
        Task { [weak self] in
            do {
                // receiveJit is a blocking FFI call — run off the MainActor.
                try await Self.runBlockingFFI {
                    try wallet.receiveJit(amountMsat: amountSats * 1_000)
                }
            } catch {
                self?.lastOutcome = "Invoice request failed: \(error.localizedDescription)"
            }
            self?.busy = false
        }
    }

    /// Passes the BOLT11 string straight to the core, which parses and
    /// validates it (R4: no invoice parsing in Swift).
    func sendPayment(bolt11: String) {
        guard let wallet, !busy else { return }
        let trimmed = bolt11.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else { return }
        busy = true
        lastOutcome = "Sending…"
        Task { [weak self] in
            do {
                // send is a blocking FFI call — run off the MainActor.
                try await Self.runBlockingFFI { try wallet.send(bolt11: trimmed) }
            } catch {
                self?.lastOutcome = "Send failed: \(error.localizedDescription)"
            }
            self?.busy = false
        }
    }

    func refreshBalances() {
        guard let wallet else { return }
        Task { [weak self] in
            do {
                // balances() crosses the blocking FFI — run off the MainActor.
                let msat = try await Self.runBlockingFFI {
                    try wallet.balances().lightningMsat
                }
                self?.balanceMsat = msat
            } catch {
                self?.lastOutcome = "Balance refresh failed: \(error.localizedDescription)"
            }
        }
    }

    // MARK: Event loop (handle-then-ack, KTD-8)

    /// Consumes nextEvent() (Kotlin suspend → Swift async) until the terminal
    /// NodeStopped: each event is reduced BEFORE it is acked, so a crash in
    /// between redelivers the same event on restart. Restarted by `start()`.
    private func runEventLoop(_ wallet: Wallet) async {
        while true {
            let event: WalletEvent
            do {
                // Kotlin suspend exports as async throws; it only throws on
                // task cancellation, which we treat as loop exit.
                event = WalletEvent.from(try await wallet.nextEvent())
            } catch {
                return
            }
            reduce(event)
            do {
                try wallet.eventHandled()
            } catch {
                lastOutcome = "Event ack failed: \(error.localizedDescription)"
            }
            if case .nodeStopped = event { return }
        }
    }

    private func reduce(_ event: WalletEvent) {
        switch event {
        case .nodeStarted:
            running = true
            refreshBalances()
        case .nodeStopped:
            running = false
        case .syncFailed:
            syncBanner = "Chain sync failed — retrying…"
        case .syncCompleted:
            syncBanner = nil
            refreshBalances()
        case let .invoiceReady(bolt11, expiryUnixSecs):
            currentInvoice = Invoice(bolt11: bolt11, expiryUnixSecs: expiryUnixSecs)
        case let .paymentReceived(amountMsat, skimmedFeeMsat):
            currentInvoice = nil
            if let skimmedFeeMsat {
                lastOutcome = "Received \(amountMsat) msat (LSP fee \(skimmedFeeMsat) msat)"
            } else {
                lastOutcome = "Received \(amountMsat) msat"
            }
            refreshBalances()
        case .paymentSuccessful:
            lastOutcome = "Payment sent"
            refreshBalances()
        case let .paymentFailed(reason):
            lastOutcome = "Payment failed: \(reason)"
        case .channelPending:
            lastOutcome = "Channel opening…"
        case .channelReady:
            lastOutcome = "Channel ready"
            refreshBalances()
        case let .lsps2Failed(reason):
            currentInvoice = nil
            lastOutcome = "LSP failed: \(reason)"
        case .fenced:
            // The core fenced itself (KTD-3): the shell blocks every
            // destination behind the fenced screen until the user restores
            // or quits (U18); never cleared by an event.
            fenced = true
        case .unknown:
            break
        }
    }

    /// Kotlin exceptions cross the Kotlin/Native bridge as NSError with the
    /// original throwable under `KotlinException`; a typed `Fenced` start
    /// failure means the durable fence is set.
    private static func isFencedError(_ error: Error) -> Bool {
        (error as NSError).userInfo["KotlinException"] is WalletException.Fenced
    }

    // MARK: Storage

    private func ensureWallet() throws -> Wallet {
        if let wallet { return wallet }
        let dir = try walletStorageDirectory()
        // Kotlin default args don't export to Swift: pass the URL overrides
        // explicitly (nil = production defaults).
        let created = WalletCore.shared.create(
            storageDir: dir.path,
            esploraUrl: nil,
            rgsUrl: nil
        )
        wallet = created
        return created
    }

    /// App-private Application Support/wallet (NOT Caches — the OS may purge
    /// Caches, and losing channel monitors is a funds hazard), excluded from
    /// iCloud/device backup (R6): a restored stale monitor set is the
    /// force-close/penalty hazard the fresh-wallet decision exists to avoid.
    private func walletStorageDirectory() throws -> URL {
        let support = try FileManager.default.url(
            for: .applicationSupportDirectory,
            in: .userDomainMask,
            appropriateFor: nil,
            create: true
        )
        var dir = support.appendingPathComponent("wallet", isDirectory: true)
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        var values = URLResourceValues()
        values.isExcludedFromBackup = true
        try dir.setResourceValues(values)
        return dir
    }
}
