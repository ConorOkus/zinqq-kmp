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
    /// `paymentHash` lets the Receive visit settle only on ITS invoice (U21).
    case paymentReceived(paymentHash: String, amountMsat: UInt64, skimmedFeeMsat: UInt64?)
    /// `paymentHash` lets the Send dispatch settle only on ITS payment (F1):
    /// the core's 5-minute outcome cap deliberately leaves a timed-out payment
    /// in flight, so without the hash the next send inherits the previous
    /// payment's outcome. Always present for a successful outbound payment.
    case paymentSuccessful(paymentHash: String)
    /// `paymentHash` is nil for a BOLT12 payment that failed before an invoice
    /// arrived (there is no hash yet) — see `Event.PaymentFailed`.
    case paymentFailed(paymentHash: String?, reason: String)
    case channelPending
    case channelReady
    case lsps2Failed(reason: String)
    /// Outputs entered/left the sweep pipeline (U11): re-query pendingSweep.
    case sweepStateChanged
    /// The force-close recovery state machine moved (U10): re-query it and
    /// invalidate any session-local banner dismissal.
    case recoveryStateChanged
    /// An on-chain (bdk) sync pass found real news — a new transaction, a
    /// confirmation, or a mempool eviction (U8): re-query balances and activity.
    case onchainStateChanged
    /// Another client took over this seed's VSS namespace (KTD-3, plan
    /// System-Wide Impact): the core fenced itself durably and halted.
    case fenced(detail: String)
    /// Restore-from-seed progress (U22, F3): the PWA's exact step copy,
    /// reduced into `WalletModel.restore` while a restore is in progress.
    case restoreProgress(step: String)
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
            return .paymentReceived(
                paymentHash: e.paymentHash,
                amountMsat: e.amountMsat,
                skimmedFeeMsat: e.skimmedFeeMsat?.uint64Value
            )
        case let e as Event.PaymentSuccessful:
            return .paymentSuccessful(paymentHash: e.paymentHash)
        case let e as Event.PaymentFailed:
            // payment_hash is Option<String> in Rust (nil for a BOLT12
            // failure before the invoice request produced an invoice).
            return .paymentFailed(paymentHash: e.paymentHash, reason: e.reason)
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
        case is Event.OnchainStateChanged:
            return .onchainStateChanged
        case let e as Event.Fenced:
            return .fenced(detail: e.detail)
        case let e as Event.RestoreProgress:
            return .restoreProgress(step: e.step)
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
///
/// `onchainStateChanged` is the ON-CHAIN half, and it is not optional: the
/// core's bdk sync tick is the only thing that learns about an on-chain receive
/// or confirmation, and nothing else fires when it does. Without it a recovered
/// sweep sat in the persisted changeset while this model kept a stale balance
/// until an unrelated Lightning event or a relaunch — the exact symptom observed
/// once the manual refresh button was removed. `syncCompleted` is NOT a
/// substitute: it only fires on a failed→healthy transition.
func shouldRefreshWalletData(_ event: WalletEvent) -> Bool {
    switch event {
    case .paymentReceived, .paymentSuccessful, .channelReady,
         .sweepStateChanged, .recoveryStateChanged, .onchainStateChanged:
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

// MARK: - Lifecycle seam

/// Minimal seam over the generated Wallet's two blocking lifecycle calls: the
/// serialized start/stop chain consumes only this protocol, so the ordering
/// regression test can drive rapid background→foreground flips against a fake
/// node instead of the real FFI — the same seam discipline as
/// `WalletEventSource` above.
protocol WalletLifecycle: AnyObject {
    /// Blocking `start()`; raises the typed `AlreadyRunning` when the core's
    /// state mutex finds the node already up.
    func startNode() throws
    /// Blocking `stop()` (up to ~20 s while the channel manager persists);
    /// raises the typed `NotRunning` when the node is already down.
    func stopNode() throws
}

extension Wallet: WalletLifecycle {
    func startNode() throws { try start() }
    func stopNode() throws { try stop() }
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
    @Published private(set) var running = false
    @Published private(set) var balanceMsat: UInt64 = 0
    @Published private(set) var lastOutcome: String?
    @Published private(set) var syncBanner: String?
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
    /// The Restore flow's live phase (U22, F3); nil = no restore running.
    /// Owned here, not on the screen, because the model owns the whole
    /// stop → restore → restart sequence: leaving the screen mid-restore
    /// cannot orphan a stopped node, and the screen re-attaches to whatever
    /// phase is current (Android's `UiState.restore` twin).
    @Published private(set) var restore: RestoreUi?
    /// Cached `nodeId()` for the Advanced screen (U22): fetched on refresh
    /// until cached — it needs a running node — but the pubkey is stable for
    /// the wallet's lifetime, so it stays readable across stops. A restore
    /// clears it so the new wallet's id is re-fetched.
    @Published private(set) var nodeId: String?
    /// Fatal start failure — Home replaces its content with the PWA's
    /// "Something went wrong" state (`Home.tsx:29-42`).
    @Published private(set) var startError: String?
    /// Persisted `balance-visible` toggle (R12), PWA localStorage key parity.
    @Published var balanceVisible: Bool =
        (UserDefaults.standard.object(forKey: "balance-visible") as? Bool) ?? true {
        didSet { UserDefaults.standard.set(balanceVisible, forKey: "balance-visible") }
    }

    private var wallet: Wallet?
    /// The lifecycle state the SHELL wants, set before each blocking
    /// transition (never after it) so a transition that interleaves with a
    /// blocking one observes it instead of being swallowed.
    private var startRequested = false
    /// Whether the app is backgrounded right now (KTD-10 foreground-only
    /// node). Injected so the lifecycle-ordering test can drive the
    /// foreground-only invariant without reaching into UIKit.
    private let isBackgrounded: () -> Bool

    init(
        isBackgrounded: @escaping () -> Bool = {
            UIApplication.shared.applicationState == .background
        }
    ) {
        self.isBackgrounded = isBackgrounded
    }

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

    /// The one serialized lifecycle chain (Android's `lifecycleJob` twin, and
    /// the same chaining discipline `scheduleEventLoop` uses for the event
    /// loop). Every start, stop and restore enqueues onto it, so two
    /// transitions can never reach the core's state mutex out of order: on a
    /// quick background→foreground flip they serialize instead of racing.
    ///
    /// Unordered detached tasks (what this replaced) let a start land while an
    /// in-flight stop was still draining — the start then saw `AlreadyRunning`
    /// and the stop finished afterwards, leaving a foregrounded app with a
    /// stopped node and no further trigger to restart it; the inverse left a
    /// running node with `startRequested == false`, which made the next
    /// `stop()` return early and vanish. Internal so the ordering regression
    /// test can await the chain's completion.
    private(set) var lifecycleTask: Task<Void, Never>?

    /// Called on scenePhase .active. Starts the node and schedules the event
    /// loop; peer reconnect after a suspend is the core's job.
    func start() {
        // A running restore owns the node lifecycle (stop → restore →
        // restart); a foreground start racing it would make the core's
        // stopped-only restore() fail with AlreadyRunning (U22, F3).
        if case .inProgress = restore { return }
        guard !startRequested else { return }
        let wallet: Wallet
        do {
            wallet = try ensureWallet()
        } catch {
            lastOutcome = "Start failed: \(error.localizedDescription)"
            startError = error.localizedDescription
            return
        }
        startNode(wallet, eventSource: wallet)
    }

    /// `start()`'s seam-taking half: enqueue the blocking start on the
    /// lifecycle chain. Internal so the ordering regression test can drive
    /// rapid start/stop sequences against a fake node.
    func startNode(_ lifecycle: WalletLifecycle, eventSource: WalletEventSource) {
        guard !startRequested else { return }
        // Requested BEFORE the blocking start, not after it: a `stop()` that
        // interleaves must be able to SEE a start in flight, otherwise its
        // `guard startRequested` early-returns and the stop is swallowed —
        // leaving a headless node running in the background.
        startRequested = true
        let previous = lifecycleTask
        lifecycleTask = Task { [weak self] in
            await previous?.value
            await self?.runStart(lifecycle, eventSource: eventSource)
        }
    }

    /// Called on scenePhase .background. `stop()` pushes the terminal
    /// NodeStopped event, which completes a pending nextEvent and lets the
    /// loop exit cleanly. It can block ~20s while the channel manager
    /// persists, so it runs off the MainActor under a UIApplication background
    /// task assertion — otherwise iOS could suspend the process mid-persist.
    func stop() {
        guard let wallet else { return }
        stopNode(wallet)
    }

    /// `stop()`'s seam-taking half. Internal for the ordering regression test.
    func stopNode(_ lifecycle: WalletLifecycle) {
        guard startRequested else { return }
        startRequested = false
        // Taken HERE, synchronously on the transition — begun after the
        // chain's awaits instead, iOS could already have suspended us.
        let endAssertion = beginStopAssertion()
        let previous = lifecycleTask
        lifecycleTask = Task { [weak self] in
            // Order behind a start still inside its blocking FFI so the core's
            // state mutex sees the two transitions in the order the scene
            // phases requested them.
            await previous?.value
            await self?.runStop(lifecycle)
            endAssertion()
        }
    }

    /// The serialized start body: the blocking FFI, then a re-read of the
    /// world it returned into.
    private func runStart(
        _ lifecycle: WalletLifecycle,
        eventSource: WalletEventSource
    ) async {
        do {
            // start() is a blocking FFI call — run it off the MainActor.
            try await Self.runBlockingFFI { try lifecycle.startNode() }
        } catch {
            if Self.isAlreadyRunningError(error) {
                // A no-op success, exactly like Android's `startCore`: the
                // node is up, which is all this call wanted. Clearing
                // `startRequested` on this benign race (a foreground flip that
                // lands while the previous stop still drains) would strand a
                // stopped node with no trigger until the next transition.
            } else {
                startRequested = false
                if Self.isFencedError(error) {
                    // The durable fence survives restart (KTD-3): a fenced
                    // wallet refuses to start, so the shell re-raises the
                    // fenced screen even though no Event.Fenced will arrive
                    // on this run (U18).
                    fenced = true
                } else {
                    lastOutcome = "Start failed: \(error.localizedDescription)"
                    startError = error.localizedDescription
                }
                return
            }
        }
        startError = nil
        // Single-consumer contract (the U19 P2 fix): the loop is only ever
        // (re)scheduled after a successful start, and scheduling chains onto
        // the previous run instead of cancelling it. Scheduled BEFORE the
        // re-check below so a compensating stop's terminal NodeStopped has a
        // live consumer to drain it.
        scheduleEventLoop(eventSource)
        // The blocking start just held this decision for seconds: if a stop
        // was requested meanwhile, or the app went to background (a scenePhase
        // stop that `stop()` dropped because the start had not landed yet),
        // that wins — otherwise a node keeps running headless against the
        // foreground-only invariant the persistence design assumes (KTD-10).
        if !startRequested || isBackgrounded() {
            startRequested = false
            let endAssertion = beginStopAssertion()
            await runStop(lifecycle)
            endAssertion()
            return
        }
        refreshWalletData()
    }

    /// The serialized stop body. `NotRunning` is a no-op success: the
    /// post-start re-check above may already have drained the node, and a
    /// user-visible "Stop failed" for that would be noise.
    private func runStop(_ lifecycle: WalletLifecycle) async {
        do {
            try await Self.runBlockingFFI { try lifecycle.stopNode() }
        } catch {
            guard !(kotlinThrowable(error) is WalletException.NotRunning) else { return }
            lastOutcome = "Stop failed: \(error.localizedDescription)"
        }
    }

    /// A UIApplication background-task assertion held across a blocking stop,
    /// returning its (idempotent) ender. Must be begun synchronously on the
    /// transition — the stop itself can block ~20s while the channel manager
    /// persists, and without the assertion iOS could suspend the process
    /// mid-persist.
    private func beginStopAssertion() -> () -> Void {
        var assertion = UIBackgroundTaskIdentifier.invalid
        let endAssertion: () -> Void = {
            guard assertion != .invalid else { return }
            UIApplication.shared.endBackgroundTask(assertion)
            assertion = .invalid
        }
        assertion = UIApplication.shared.beginBackgroundTask(withName: "wallet-stop") {
            // Expiration: iOS reclaims the assertion; nothing to cancel —
            // stop() is not interruptible — just release the token.
            endAssertion()
        }
        return endAssertion
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
            // Cached across stops (U22): nodeId() needs a running node, but
            // the pubkey is stable for the wallet's lifetime — fetch it only
            // until it caches (startRestore clears it for the new wallet).
            let freshNodeId = self?.nodeId == nil
                ? (try? await Self.runBlockingFFI { try wallet.nodeId() })
                : nil
            guard let self else { return }
            if let balances {
                self.balances = balances
                self.balanceMsat = balances.lightningMsat
            }
            self.nodeId = freshNodeId ?? self.nodeId
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
        case .invoiceReady:
            // The receive flow renders invoices from its own controller
            // (U21); nothing to reduce here.
            break
        case let .paymentReceived(_, amountMsat, skimmedFeeMsat):
            // Optimistic bookkeeping; the triggered refreshWalletData()
            // overwrites it with the authoritative balances() snapshot.
            balanceMsat += amountMsat
            if let skimmedFeeMsat {
                lastOutcome = "Received \(amountMsat / 1_000) sats (LSP fee \(skimmedFeeMsat / 1_000) sats)"
            } else {
                lastOutcome = "Received \(amountMsat / 1_000) sats"
            }
        case .paymentSuccessful:
            lastOutcome = "Payment sent"
        case let .paymentFailed(_, reason):
            lastOutcome = "Payment failed: \(reason)"
        case .channelPending:
            lastOutcome = "JIT channel opening"
        case .channelReady:
            lastOutcome = "Channel ready"
        case let .lsps2Failed(reason):
            lastOutcome = "Invoice request failed: \(reason)"
        case .sweepStateChanged:
            // No direct state: the triggered refresh re-queries pendingSweep.
            break
        case .recoveryStateChanged:
            // Fresh recovery state invalidates a session-local banner
            // dismissal (the triggered refresh re-queries the state itself).
            recoveryBannerDismissed = false
        case .onchainStateChanged:
            // No direct state: the triggered refresh re-queries balances() and
            // the activity list, which are the authoritative on-chain view.
            break
        case .fenced:
            // The core fenced itself (KTD-3): the shell blocks every
            // destination behind the fenced screen until the user restores
            // or quits (U18); never cleared by an event.
            fenced = true
        case .restoreProgress:
            // U22/F3: only advances an in-progress restore — a stray late
            // event can neither start one nor resurrect a terminal outcome.
            restore = reduceRestore(restore, event)
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

    /// A start that found the node already up — a no-op success, not a
    /// failure (Android's `startCore` parity).
    private static func isAlreadyRunningError(_ error: Error) -> Bool {
        kotlinThrowable(error) is WalletException.AlreadyRunning
    }

    // MARK: Restore lifecycle (U22, F3)

    /// F3: replace the current wallet from 12 validated words, mirroring
    /// Android's `WalletHolder.startRestore` adapted to this model's chained
    /// event loop (U19). The core's `restore()` is valid only from a stopped
    /// node, so the model owns the whole sequence:
    ///
    /// 1. `stop()` — pushes the terminal NodeStopped, which lets the current
    ///    loop run drain and exit (a NotRunning stop pushes nothing).
    /// 2. `scheduleEventLoop(wallet)` — Android's `ensureEventLoop` twin:
    ///    the chained scheduling first awaits the old run's exit (the
    ///    `loopJob.join()` half), then the fresh run parks in `nextEvent`
    ///    and drains `RestoreProgress` events live while `restore()` blocks.
    /// 3. Blocking `restore()` off the MainActor.
    /// 4. `restartAfterRestore` — foreground-gated `start()` WITHOUT another
    ///    `scheduleEventLoop`: the restore's run is still the live consumer
    ///    (it only exits on the next NodeStopped), exactly like Android's
    ///    idempotent `ensureEventLoop` no-op.
    ///
    /// Runs on the lifecycle chain, unscoped from any screen: leaving the
    /// screen mid-restore cannot orphan a stopped node, the screen re-attaches
    /// to whatever phase is current, and a scenePhase transition that arrives
    /// mid-restore orders strictly after the whole sequence instead of racing
    /// its blocking calls.
    func startRestore(mnemonic: String) {
        if case .inProgress = restore { return }
        restore = .inProgress(step: restoreInitialStep)
        let previous = lifecycleTask
        lifecycleTask = Task { [weak self] in
            await previous?.value
            await self?.runRestore(mnemonic: mnemonic)
        }
    }

    private func runRestore(mnemonic: String) async {
        let wallet: Wallet
        do {
            wallet = try ensureWallet()
        } catch {
            restore = .failed(message: restoreErrorMessage(error))
            return
        }
        // The restore sequence owns the node lifecycle from here: the node is
        // (about to be) stopped, so scenePhase stop() must not race it, and
        // start() is gated on the in-progress restore.
        startRequested = false
        do {
            try await Self.runBlockingFFI { try wallet.stop() }
        } catch {
            // NotRunning: nothing to stop (pushes nothing). A stop with a
            // failed final persist still transitioned and still pushed
            // the terminal NodeStopped (see api.rs stop()).
        }
        // Drain RestoreProgress events live while restore() blocks.
        scheduleEventLoop(wallet)
        do {
            try await Self.runBlockingFFI { try wallet.restore(mnemonic: mnemonic) }
        } catch {
            // The typed failures leave local state untouched — restart
            // the existing wallet so the app stays usable behind the
            // error screen (a still-fenced wallet re-fences here).
            await restartAfterRestore(wallet)
            restore = .failed(message: restoreErrorMessage(error))
            return
        }
        // The restored wallet replaced the old one — any fence fell with
        // it (a start failure still surfaces through startError on Home),
        // and its node id must be re-fetched.
        fenced = false
        nodeId = nil
        await restartAfterRestore(wallet)
        restore = .succeeded
    }

    /// Restore's exit restart, foreground-gated (KTD-10): a restore that
    /// finishes while the app is backgrounded must not leave a headless node
    /// running past its missed scenePhase stop — the next foreground start
    /// covers it. Deliberately does NOT schedule another event-loop run: the
    /// restore's chained run is still the single live consumer, so it drains a
    /// compensating stop's terminal NodeStopped too.
    private func restartAfterRestore(_ lifecycle: WalletLifecycle) async {
        guard !isBackgrounded() else { return }
        // Requested BEFORE the blocking start, not after it: set afterwards, a
        // scenePhase stop that interleaves hits `guard startRequested` while
        // the flag is still false and vanishes — leaving the restored node
        // running headless in the background (KTD-10).
        startRequested = true
        do {
            try await Self.runBlockingFFI { try lifecycle.startNode() }
        } catch {
            if Self.isAlreadyRunningError(error) {
                // A no-op success (Android's startCore parity).
            } else {
                startRequested = false
                if Self.isFencedError(error) {
                    fenced = true
                } else {
                    startError = error.localizedDescription
                }
                return
            }
        }
        startError = nil
        // Same post-start re-read as `runStart`: a stop requested while
        // restore's start blocked, or a background transition we missed, wins.
        if !startRequested || isBackgrounded() {
            startRequested = false
            let endAssertion = beginStopAssertion()
            await runStop(lifecycle)
            endAssertion()
            return
        }
        refreshWalletData()
    }

    /// The Restore screen's Try-Again/exit ack; a running restore stays owned.
    func clearRestore() {
        if case .inProgress = restore { return }
        restore = nil
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

// MARK: - ReceivePort (U21, R14)

/// Thin passthroughs to the core's receive FFI, mirroring Android's
/// `WalletHolder` ReceivePort section: the capacity decision, live floor,
/// quote/buy protocol, and expiry clamp all live in Rust; blocking calls hop
/// off the MainActor via `runBlockingFFI`. `walletEvents` (shared with
/// SendPort) already satisfies the protocol's event stream.
extension WalletModel: ReceivePort {
    func receiveBundle(amountMsat: UInt64?) async throws -> ReceiveBundle {
        let wallet = try requireWallet()
        return try await Self.runBlockingFFI {
            try wallet.receiveBundle(
                amountMsat: amountMsat.map { KotlinULong(unsignedLongLong: $0) }
            )
        }
    }

    func jitQuote(amountMsat: UInt64) async throws -> JitQuote {
        let wallet = try requireWallet()
        return try await Self.runBlockingFFI {
            try wallet.jitQuote(amountMsat: amountMsat)
        }
    }

    func jitAccept(quoteToken: UInt64, amountMsat: UInt64) async throws -> JitInvoice {
        let wallet = try requireWallet()
        return try await Self.runBlockingFFI {
            try wallet.jitAccept(quoteToken: quoteToken, amountMsat: amountMsat)
        }
    }

    func minReceiveSats(refresh: Bool) async throws -> UInt64 {
        let wallet = try requireWallet()
        return try await Self.runBlockingFFI {
            wallet.minReceiveSats(refresh: refresh)
        }
    }

    func usableInboundMsat() async throws -> UInt64 {
        let wallet = try requireWallet()
        let channels = try await Self.runBlockingFFI { try wallet.listChannels() }
        return sumUsableInboundMsat(channels)
    }

    func buildUnifiedUri(
        address: String,
        amountSats: UInt64?,
        invoice: String?
    ) async throws -> String {
        try await Self.runBlockingFFI {
            Wallet_core_nativeKt.buildBip321Uri(
                address: address,
                amountSats: amountSats.map { KotlinULong(unsignedLongLong: $0) },
                invoice: invoice
            )
        }
    }

    // Blocking through the core's 3/6/12/24/48 s offer-creation retries, so
    // the caller keeps it off the receive entry path (ReceiveController).
    func getOrCreateOffer() async throws -> String? {
        let wallet = try requireWallet()
        return try await Self.runBlockingFFI { wallet.getOrCreateOffer() }
    }

    func bolt12Uri(offer: String) async throws -> String {
        try await Self.runBlockingFFI { Wallet_core_nativeKt.buildBolt12Uri(offer: offer) }
    }

    // Non-blocking in the core (LDK owns the retry schedule and persistence),
    // but kept off the main actor for consistency with its neighbours. One
    // call per visit: the core's read consumes an offer from LDK's cache.
    func asyncReceive() async throws -> AsyncReceiveView {
        let wallet = try requireWallet()
        return try await Self.runBlockingFFI { wallet.asyncReceive() }
    }
}

// MARK: - SettingsPort (U22, R14)

/// Thin passthroughs to the core's mnemonic and channels/peers FFI,
/// mirroring Android's `WalletHolder` SettingsPort section: bounds, guards,
/// close estimates, and the connect protocol all live in Rust; blocking
/// calls hop off the MainActor via `runBlockingFFI`.
extension WalletModel: SettingsPort {
    func revealMnemonic() async throws -> String {
        let wallet = try requireWallet()
        return try await Self.runBlockingFFI { try wallet.revealMnemonic() }
    }

    func validateMnemonic(_ mnemonic: String) async -> Bool {
        // deriveDebugInfo is the exported BIP39 check (U1): it fails typed
        // (InvalidMnemonic) on anything but valid 12 English words.
        (try? await Self.runBlockingFFI {
            try Wallet_core_nativeKt.deriveDebugInfo(mnemonic: mnemonic)
        }) != nil
    }

    func listPeers() async throws -> [PeerView] {
        let wallet = try requireWallet()
        return try await Self.runBlockingFFI { try wallet.listPeers() }
    }

    func listChannels() async throws -> [ChannelView] {
        let wallet = try requireWallet()
        return try await Self.runBlockingFFI { try wallet.listChannels() }
    }

    func forgetPeer(pubkey: String) async throws {
        let wallet = try requireWallet()
        try await Self.runBlockingFFI { try wallet.forgetPeer(pubkey: pubkey) }
    }

    func openChannel(peerAddress: String, amountSats: UInt64) async throws -> String {
        let wallet = try requireWallet()
        return try await Self.runBlockingFFI {
            try wallet.openChannel(peerAddress: peerAddress, amountSats: amountSats)
        }
    }

    func estimateOpenFee() async throws -> OpenFeeEstimate {
        let wallet = try requireWallet()
        return try await Self.runBlockingFFI { try wallet.estimateOpenFee() }
    }

    func estimateClose(channelId: String) async throws -> CloseEstimate {
        let wallet = try requireWallet()
        return try await Self.runBlockingFFI { wallet.estimateClose(channelId: channelId) }
    }

    func closeChannel(channelId: String, force: Bool) async throws {
        let wallet = try requireWallet()
        try await Self.runBlockingFFI {
            try wallet.closeChannel(channelId: channelId, force: force)
        }
    }

    // `onchainBalanceSats()` is shared with SendPort above: the PWA's
    // trusted-spendable on-chain figure (total − untrusted pending).
}
