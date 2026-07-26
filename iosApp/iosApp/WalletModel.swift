import Combine
import Foundation
import Shared

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
    case paymentReceived(amountMsat: UInt64, skimmedFeeMsat: UInt64)
    case paymentSuccessful
    case paymentFailed(reason: String)
    case channelPending
    case channelReady
    case lsps2Failed(reason: String)
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
            return .paymentReceived(amountMsat: e.amountMsat, skimmedFeeMsat: e.skimmedFeeMsat)
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

    private var wallet: Wallet?
    private var eventLoop: Task<Void, Never>?
    private var startRequested = false

    // MARK: Lifecycle (KTD-10: foreground-only node)

    /// Called on scenePhase .active. Starts the node and (re)starts the event
    /// loop; peer reconnect after a suspend is the core's job.
    func start() {
        guard !startRequested else { return }
        do {
            let wallet = try ensureWallet()
            try wallet.start()
            startRequested = true
            // The previous loop (if any) exited on NodeStopped, or is still
            // parked on a stale nextEvent — cancel it and start fresh.
            // Cancellation can only land while awaiting nextEvent, before an
            // event is handled, so no event is lost (unacked events redeliver).
            eventLoop?.cancel()
            eventLoop = Task { [weak self] in
                await self?.runEventLoop(wallet)
            }
        } catch {
            lastOutcome = "Start failed: \(error.localizedDescription)"
        }
    }

    /// Called on scenePhase .background. `stop()` pushes the terminal
    /// NodeStopped event, which completes a pending nextEvent and lets the
    /// loop exit cleanly.
    func stop() {
        guard startRequested, let wallet else { return }
        startRequested = false
        do {
            try wallet.stop()
        } catch {
            lastOutcome = "Stop failed: \(error.localizedDescription)"
        }
    }

    // MARK: Intents

    /// Requests a Megalith JIT invoice; the invoice arrives asynchronously as
    /// InvoiceReady (or Lsps2Failed). Sats→msat is unit scaling only, not fee
    /// math (R4).
    func requestInvoice(amountSats: UInt64) {
        guard let wallet else { return }
        do {
            try wallet.receiveJit(amountMsat: amountSats * 1_000)
            lastOutcome = nil
        } catch {
            lastOutcome = "Invoice request failed: \(error.localizedDescription)"
        }
    }

    /// Passes the BOLT11 string straight to the core, which parses and
    /// validates it (R4: no invoice parsing in Swift).
    func sendPayment(bolt11: String) {
        guard let wallet else { return }
        let trimmed = bolt11.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else { return }
        do {
            try wallet.send(bolt11: trimmed)
            lastOutcome = "Sending…"
        } catch {
            lastOutcome = "Send failed: \(error.localizedDescription)"
        }
    }

    func refreshBalances() {
        guard let wallet else { return }
        do {
            balanceMsat = try wallet.balances().lightningMsat
        } catch {
            lastOutcome = "Balance refresh failed: \(error.localizedDescription)"
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
            lastOutcome = "Received \(amountMsat) msat (LSP fee \(skimmedFeeMsat) msat)"
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
        case .unknown:
            break
        }
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
