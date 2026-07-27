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
    /// Outputs entered/left the sweep pipeline (U11): re-query pendingSweep.
    case sweepStateChanged
    /// The force-close recovery state machine moved (U10): re-query it and
    /// invalidate any session-local banner dismissal.
    case recoveryStateChanged
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
        case is Event.SweepStateChanged:
            return .sweepStateChanged
        case is Event.RecoveryStateChanged:
            return .recoveryStateChanged
        case let e as Event.Fenced:
            return .fenced(detail: e.detail)
        default:
            return .unknown
        }
    }
}

// MARK: - Refresh triggers

/// The events after which the wallet-data snapshots (balances, activity,
/// recovery state, pending sweep) must be re-queried: the spike's balance
/// triggers extended with the sweep/recovery change events (U19; identical to
/// Android's `shouldRefreshWalletData` — the PWA's hooks re-read on the
/// equivalent change notifications).
func shouldRefreshWalletData(_ event: WalletEvent) -> Bool {
    switch event {
    case .paymentReceived, .paymentSuccessful, .channelReady,
         .sweepStateChanged, .recoveryStateChanged:
        return true
    default:
        return false
    }
}

// MARK: - Event source seam

/// Minimal seam over the generated Wallet's event queue (U19): the event loop
/// consumes only this protocol, so the single-consumer regression test can
/// drive the loop with a fake queue instead of the blocking FFI.
protocol WalletEventSource: AnyObject {
    /// Kotlin suspend `nextEvent()` adapted to the Swift-side event enum.
    func nextWalletEvent() async throws -> WalletEvent
    /// Handle-then-ack (KTD-8): acks the last event returned by next.
    func ackEvent() async throws
}

extension Wallet: WalletEventSource {
    func nextWalletEvent() async throws -> WalletEvent {
        WalletEvent.from(try await nextEvent())
    }

    func ackEvent() async throws {
        try eventHandled()
    }
}

// MARK: - Model

/// The channel-close detail query result (U19): `record` is nil when the
/// core has no record for `channelId` ("Close record not found"), while a
/// missing `CloseDetailUi` altogether means the query hasn't run yet.
struct CloseDetailUi {
    let channelId: String
    let record: CloseRecordView?
}

/// Owns the shared `Wallet` and its handle-then-ack event loop, reducing
/// events into published UI state. No Lightning logic lives here (R14):
/// strings go in, events come out — the core does all parsing, fees, and
/// reconnect (KTD-10). Display derivations live in `WalletPresentation.swift`
/// as pure functions; this class only snapshots core queries.
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

    // MARK: Wallet-data snapshots (U19)

    /// Last `balances()` snapshot; nil until the first refresh (loading).
    @Published private(set) var balances: Balances?
    /// Last `listActivity()` snapshot; nil until the first refresh (loading).
    @Published private(set) var activity: [ActivityRow]?
    /// Force-close recovery state; nil = no recovery in progress (R9).
    @Published private(set) var recoveryState: RecoveryStateView?
    /// Session-local hide of the sweep-confirmed success banner. The durable
    /// half is the core's `dismissRecovery()`; the session flag hides the
    /// banner immediately and resets whenever `Event.RecoveryStateChanged`
    /// announces fresh state.
    @Published private(set) var recoveryBannerDismissed = false
    /// Outputs waiting to sweep; the banner gates on `lastAttemptFailed` (R8).
    @Published private(set) var pendingSweep: PendingSweepView?
    /// The close-detail screen's current query result.
    @Published private(set) var closeDetail: CloseDetailUi?
    /// Fatal start failure — Home replaces its content with the PWA's
    /// "Something went wrong" state (`Home.tsx:29-42`).
    @Published private(set) var startError: String?
    /// Persisted `balance-visible` toggle (R12), PWA localStorage key parity.
    @Published var balanceVisible: Bool =
        (UserDefaults.standard.object(forKey: "balance-visible") as? Bool) ?? true {
        didSet { UserDefaults.standard.set(balanceVisible, forKey: "balance-visible") }
    }

    private var wallet: Wallet?
    private var startRequested = false

    // MARK: Live event rebroadcast (U20)

    /// U20: live rebroadcast of core events so the send flow can await its
    /// payment outcome (F1). No replay — a subscriber must exist before the
    /// dispatch it cares about (the durable queue in the core is the real
    /// record); Android's `WalletHolder._events` twin.
    private let eventsSubject = PassthroughSubject<WalletEvent, Never>()

    // MARK: Blocking FFI dispatch

    /// Runs a blocking Wallet FFI call off the MainActor — rust/src/api.rs
    /// documents start/receive_jit/send as blocking and stop() as blocking up
    /// to ~20s — mirroring the Android shell's Dispatchers.IO wrapping.
    /// Callers hop back to the MainActor (their own isolation) to publish
    /// state.
    private static func runBlockingFFI<T>(
        _ body: @escaping () throws -> T
    ) async throws -> T {
        try await Task.detached(priority: .userInitiated) {
            try body()
        }.value
    }

    // MARK: Lifecycle (KTD-10: foreground-only node)

    /// Called on scenePhase .active. Starts the node and schedules the event
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
                self.startError = nil
                self.refreshWalletData()
                // Single-consumer contract (the U19 P2 fix): the loop is only
                // ever (re)scheduled after a successful start, and scheduling
                // chains onto the previous run instead of cancelling it.
                self.scheduleEventLoop(wallet)
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
                    self.startError = error.localizedDescription
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

    // MARK: Wallet-data queries (U19)

    /// Re-query every wallet-data snapshot the screens render (U19): balances,
    /// the unified activity feed, recovery state, pending sweep, and the open
    /// close detail, if any. Home's refresh icon calls this directly; the
    /// event loop calls it on `shouldRefreshWalletData` events. All derivation
    /// happens in pure presentation functions (R14) — this only snapshots.
    func refreshWalletData() {
        guard let wallet else { return }
        Task { [weak self] in
            // Balances/activity need a running node; keep the previous
            // snapshots when the query fails (e.g. refresh while stopped).
            let balances = try? await Self.runBlockingFFI { try wallet.balances() }
            let activity = try? await Self.runBlockingFFI { try wallet.listActivity() }
            // Local-first stores: readable even while stopped, and nil is a
            // real answer (no recovery / nothing pending), not a failure.
            let recovery = try? await Self.runBlockingFFI { wallet.recoveryState() }
            let sweep = try? await Self.runBlockingFFI { wallet.pendingSweep() }
            guard let self else { return }
            if let balances {
                self.balances = balances
                self.balanceMsat = balances.lightningMsat
            }
            self.activity = activity ?? self.activity
            self.recoveryState = recovery ?? nil
            self.pendingSweep = sweep ?? nil
            if let detail = self.closeDetail {
                let record = try? await Self.runBlockingFFI {
                    wallet.closeDetail(channelId: detail.channelId)
                }
                self.closeDetail = CloseDetailUi(
                    channelId: detail.channelId, record: record ?? nil
                )
            }
        }
    }

    /// Load (or re-load) the close-detail screen's record. The screen renders
    /// `closeDetail`; refreshes keep it live while the close resolves.
    func loadCloseDetail(channelId: String) {
        guard let wallet else {
            closeDetail = CloseDetailUi(channelId: channelId, record: nil)
            return
        }
        Task { [weak self] in
            let record = try? await Self.runBlockingFFI {
                wallet.closeDetail(channelId: channelId)
            }
            self?.closeDetail = CloseDetailUi(channelId: channelId, record: record ?? nil)
        }
    }

    /// Dismiss the sweep-confirmed success banner: durable via the core
    /// (`dismissRecovery`, a no-op unless SweepConfirmed) plus the session
    /// flag so the UI hides immediately.
    func dismissRecoveryBanner() {
        recoveryBannerDismissed = true
        guard let wallet else { return }
        Task { [weak self] in
            _ = try? await Self.runBlockingFFI { wallet.dismissRecovery() }
            self?.refreshWalletData()
        }
    }

    // MARK: Event loop (handle-then-ack KTD-8; single consumer, the U19 P2 fix)

    /// The one task allowed to touch the event queue. Internal (not private)
    /// so the regression test can await the chain's completion.
    private(set) var eventLoopTask: Task<Void, Never>?

    /// Schedules the next consumption run while preserving the queue's
    /// single-consumer contract.
    ///
    /// The old cancel-and-restart pattern could double-consume: task
    /// cancellation is cooperative, so on a quick background→foreground flip
    /// the cancelled loop could still be between `nextEvent` and
    /// `eventHandled` (or blocked past its cancellation check) while the
    /// replacement loop already awaited `nextEvent` — two live consumers on a
    /// queue whose contract is exactly one. The consumer is now never
    /// cancelled: each successful `start()` CHAINS the next run onto the
    /// previous task, so a new run first awaits the old run's exit — the old
    /// run ends by consuming the terminal NodeStopped its `stop()` pushed —
    /// and only then calls `nextEvent`. At most one run ever consumes; rapid
    /// stop/start cycles serialize instead of overlapping. Internal so the
    /// regression test can drive it against a fake `WalletEventSource`.
    func scheduleEventLoop(_ source: WalletEventSource) {
        let previous = eventLoopTask
        eventLoopTask = Task { [weak self] in
            await previous?.value
            await self?.runEventLoop(source)
        }
    }

    /// Consumes nextEvent until the terminal NodeStopped: each event is
    /// reduced BEFORE it is acked, so a crash in between redelivers the same
    /// event on restart. Restarted (chained) only by a successful `start()`.
    private func runEventLoop(_ source: WalletEventSource) async {
        while true {
            let event: WalletEvent
            do {
                // Kotlin suspend exports as async throws; it only throws on
                // task cancellation, which we treat as loop exit.
                event = try await source.nextWalletEvent()
            } catch {
                return
            }
            handle(event)
            do {
                try await source.ackEvent()
            } catch {
                lastOutcome = "Event ack failed: \(error.localizedDescription)"
            }
            if case .nodeStopped = event { return }
        }
    }

    /// Reduce + conditional wallet-data refresh; internal so tests can drive
    /// the same path the event loop takes. Rebroadcast AFTER the reduce so
    /// subscribers (the send flow's outcome await, U20) observe state and
    /// event in order — same ordering as Android's `WalletHolder`.
    func handle(_ event: WalletEvent) {
        reduce(event)
        eventsSubject.send(event)
        if shouldRefreshWalletData(event) { refreshWalletData() }
    }

    private func reduce(_ event: WalletEvent) {
        switch event {
        case .nodeStarted:
            running = true
        case .nodeStopped:
            running = false
        case .syncFailed:
            syncBanner = "Chain sync failed — retrying…"
        case .syncCompleted:
            syncBanner = nil
        case let .invoiceReady(bolt11, expiryUnixSecs):
            currentInvoice = Invoice(bolt11: bolt11, expiryUnixSecs: expiryUnixSecs)
        case let .paymentReceived(amountMsat, skimmedFeeMsat):
            // Optimistic bookkeeping; the triggered refreshWalletData()
            // overwrites it with the authoritative balances() snapshot.
            balanceMsat += amountMsat
            currentInvoice = nil
            if let skimmedFeeMsat {
                lastOutcome = "Received \(amountMsat / 1_000) sats (LSP fee \(skimmedFeeMsat / 1_000) sats)"
            } else {
                lastOutcome = "Received \(amountMsat / 1_000) sats"
            }
        case .paymentSuccessful:
            lastOutcome = "Payment sent"
        case let .paymentFailed(reason):
            lastOutcome = "Payment failed: \(reason)"
        case .channelPending:
            lastOutcome = "JIT channel opening"
        case .channelReady:
            lastOutcome = "Channel ready"
        case let .lsps2Failed(reason):
            currentInvoice = nil
            lastOutcome = "Invoice request failed: \(reason)"
        case .sweepStateChanged:
            // No direct state: the triggered refresh re-queries pendingSweep.
            break
        case .recoveryStateChanged:
            // Fresh recovery state invalidates a session-local banner
            // dismissal (the triggered refresh re-queries the state itself).
            recoveryBannerDismissed = false
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

// MARK: - SendPort (U20, R14)

/// Thin passthroughs to the core's send FFI, mirroring Android's
/// `WalletHolder` SendPort section: blocking calls hop off the MainActor via
/// `runBlockingFFI`; `resolveInput`/`fetchLnurlInvoice` are already suspend
/// bindings exported as async.
extension WalletModel: SendPort {
    private func requireWallet() throws -> Wallet {
        guard let wallet else { throw SendPortError.walletUnavailable }
        return wallet
    }

    func classify(_ input: String) async throws -> ClassifiedView {
        let wallet = try requireWallet()
        return try await Self.runBlockingFFI { wallet.classifyInput(input: input) }
    }

    func resolve(_ input: String) async throws -> ResolvedView {
        try await requireWallet().resolveInput(input: input)
    }

    func fetchLnurlInvoice(
        _ lnurl: LnurlPayView,
        amountMsat: UInt64
    ) async throws -> ClassifiedView {
        try await requireWallet().fetchLnurlInvoice(lnurl: lnurl, amountMsat: amountMsat)
    }

    func sendBolt11(_ bolt11: String, amountMsat: UInt64?) async throws {
        let wallet = try requireWallet()
        try await Self.runBlockingFFI {
            try wallet.sendBolt11(
                bolt11: bolt11,
                amountMsat: amountMsat.map { KotlinULong(unsignedLongLong: $0) }
            )
        }
    }

    func payOffer(_ offer: String, amountMsat: UInt64?) async throws {
        let wallet = try requireWallet()
        try await Self.runBlockingFFI {
            try wallet.payOffer(
                offer: offer,
                amountMsat: amountMsat.map { KotlinULong(unsignedLongLong: $0) },
                payerNote: nil
            )
        }
    }

    func estimateOnchainFee(address: String, amountSats: UInt64) async throws -> FeeEstimate {
        let wallet = try requireWallet()
        return try await Self.runBlockingFFI {
            try wallet.estimateOnchainFee(address: address, amountSats: amountSats)
        }
    }

    func estimateMaxSendable(address: String) async throws -> MaxSendEstimate {
        let wallet = try requireWallet()
        return try await Self.runBlockingFFI {
            try wallet.estimateMaxSendable(address: address)
        }
    }

    func sendOnchain(
        address: String,
        amountSats: UInt64,
        expectedAmountSats: UInt64,
        expectedFeeSats: UInt64
    ) async throws -> String {
        let wallet = try requireWallet()
        return try await Self.runBlockingFFI {
            try wallet.sendOnchain(
                address: address,
                amountSats: amountSats,
                expectedAmountSats: expectedAmountSats,
                expectedFeeSats: expectedFeeSats
            )
        }
    }

    func sendOnchainMax(
        address: String,
        expectedAmountSats: UInt64,
        expectedFeeSats: UInt64
    ) async throws -> String {
        let wallet = try requireWallet()
        return try await Self.runBlockingFFI {
            try wallet.sendOnchainMax(
                address: address,
                expectedAmountSats: expectedAmountSats,
                expectedFeeSats: expectedFeeSats
            )
        }
    }

    /// A fresh subscription per access: the sink registers synchronously at
    /// stream creation, so subscribing BEFORE dispatch cannot miss an
    /// instant outcome (Android's `_events.asSharedFlow()` twin; F1).
    var walletEvents: AsyncStream<WalletEvent> {
        let subject = eventsSubject
        return AsyncStream { continuation in
            let cancellable = subject.sink { continuation.yield($0) }
            continuation.onTermination = { _ in cancellable.cancel() }
        }
    }

    func lightningCapacityMsat() -> UInt64 {
        balances?.lightningMsat ?? 0
    }

    /// The PWA's onchainBalance is confirmed + trusted pending
    /// (`Send.tsx:164-165`) = total − untrusted pending, both core-computed.
    func onchainBalanceSats() -> UInt64 {
        guard let balances else { return 0 }
        return balances.onchainTotalSats - balances.onchainUntrustedPendingSats
    }
}
