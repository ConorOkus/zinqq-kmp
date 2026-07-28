import Foundation
import Shared
import XCTest

@testable import iosApp

/// A fake node standing in for the core's mutex-serialized start/stop. It
/// tracks the node's REAL running state, raises the same typed
/// `AlreadyRunning`/`NotRunning` outcomes the core does, counts how many
/// lifecycle calls sit inside the blocking FFI at once (the core serializes
/// behind one state mutex — the shell must not need it to), and holds each call
/// briefly so an unserialized shell interleaves observably.
final class FakeNode: WalletLifecycle, @unchecked Sendable {
    private let lock = NSLock()
    private var running: Bool
    private var starts = 0
    private var stops = 0
    private var inFlight = 0
    private var maxInFlight = 0

    /// How long each blocking call holds, widening the race window.
    let blockingSeconds: TimeInterval
    /// When true, the NEXT `startNode()` parks on `startGate` until the test
    /// signals it, so a transition can be requested WHILE a start is inside the
    /// FFI. One-shot (and time-bounded) so a later start can never park on an
    /// unsignalled gate and wedge the test.
    var gateStart: Bool {
        get { lock.withLock { gateNextStart } }
        set { lock.withLock { gateNextStart = newValue } }
    }

    let startGate = DispatchSemaphore(value: 0)
    private var gateNextStart = false

    init(running: Bool = false, blockingSeconds: TimeInterval = 0.02) {
        self.running = running
        self.blockingSeconds = blockingSeconds
    }

    var isRunning: Bool { lock.withLock { running } }
    var startCalls: Int { lock.withLock { starts } }
    var stopCalls: Int { lock.withLock { stops } }
    var maxConcurrentCalls: Int { lock.withLock { maxInFlight } }

    func startNode() throws {
        enter { starts += 1 }
        defer { leave() }
        if consumeStartGate() { _ = startGate.wait(timeout: .now() + 5) }
        Thread.sleep(forTimeInterval: blockingSeconds)
        try lock.withLock {
            if running { throw kotlinError(WalletException.AlreadyRunning()) }
            running = true
        }
    }

    func stopNode() throws {
        enter { stops += 1 }
        defer { leave() }
        Thread.sleep(forTimeInterval: blockingSeconds)
        try lock.withLock {
            guard running else { throw kotlinError(WalletException.NotRunning()) }
            running = false
        }
    }

    private func enter(_ count: () -> Void) {
        lock.withLock {
            count()
            inFlight += 1
            maxInFlight = max(maxInFlight, inFlight)
        }
    }

    private func leave() {
        lock.withLock { inFlight -= 1 }
    }

    /// Takes the one-shot gate, if armed.
    private func consumeStartGate() -> Bool {
        lock.withLock {
            defer { gateNextStart = false }
            return gateNextStart
        }
    }
}

/// The lifecycle tests are not about the event loop: this source ends each
/// scheduled run immediately (the same exit path a cancelled `nextEvent`
/// takes), so the only chain under test is the start/stop chain.
final class ClosedEventSource: WalletEventSource {
    private struct Closed: Error {}

    func nextWalletEvent() async throws -> WalletEvent { throw Closed() }
    func ackEvent() async throws {}
}

/// Regression tests for the P0 lifecycle race: `start()` and `stop()` used to
/// spawn UNORDERED detached tasks around the core's mutex-serialized FFI, so a
/// quick background→foreground flip could land them out of order —
///
///   * a start that hit `AlreadyRunning` while an in-flight stop was still
///     draining was treated as a FAILURE, which cleared `startRequested` and
///     left a foregrounded app with a stopped node and no further trigger to
///     restart it; and
///   * the inverse left a running node with `startRequested == false`, so the
///     next `stop()` returned early and vanished — a headless node against the
///     foreground-only invariant (KTD-10).
///
/// The fix is the chaining discipline `scheduleEventLoop` already used for the
/// event loop, plus setting the requested state BEFORE each blocking call and
/// re-reading it afterwards.
@MainActor
final class LifecycleOrderingTests: XCTestCase {

    /// Await the whole lifecycle chain (each link awaits its predecessor, so
    /// the latest task covers everything enqueued before this call).
    private func drain(_ model: WalletModel) async {
        await model.lifecycleTask?.value
        await Task.yield()
        await model.lifecycleTask?.value
    }

    /// The ordering invariant itself: a start and a stop must never reach the
    /// core's state mutex concurrently, and the node's final state must match
    /// the final requested state.
    func testRapidTransitionsNeverOverlapAndEndInTheRequestedState() async {
        let model = WalletModel(isBackgrounded: { false })
        let node = FakeNode()
        let source = ClosedEventSource()

        // A drawer of background/foreground flips faster than the blocking FFI.
        model.startNode(node, eventSource: source)
        model.stopNode(node)
        model.startNode(node, eventSource: source)
        model.stopNode(node)
        model.startNode(node, eventSource: source)
        await drain(model)

        XCTAssertEqual(
            1, node.maxConcurrentCalls,
            "a start and a stop reached the core's state mutex concurrently"
        )
        XCTAssertTrue(
            node.isRunning,
            "the final requested state was started, but the node ended stopped"
        )
        // Every requested transition actually reached the core — none swallowed.
        XCTAssertEqual(3, node.startCalls)
        XCTAssertEqual(2, node.stopCalls)
    }

    /// The inverse final state: a flip that ends backgrounded ends stopped.
    func testRapidTransitionsEndingInStopLeaveTheNodeStopped() async {
        let model = WalletModel(isBackgrounded: { false })
        let node = FakeNode()
        let source = ClosedEventSource()

        model.startNode(node, eventSource: source)
        model.stopNode(node)
        model.startNode(node, eventSource: source)
        model.stopNode(node)
        await drain(model)

        XCTAssertEqual(1, node.maxConcurrentCalls)
        XCTAssertFalse(node.isRunning, "a headless node survived the final stop")
    }

    /// A stop requested WHILE the blocking start is still running must not be
    /// swallowed: `startRequested` is now set before the start, so the stop
    /// sees a start in flight, and the post-start re-check honours it.
    func testStopRequestedDuringABlockingStartIsNotSwallowed() async {
        let model = WalletModel(isBackgrounded: { false })
        let node = FakeNode()
        node.gateStart = true

        model.startNode(node, eventSource: ClosedEventSource())
        model.stopNode(node)
        node.startGate.signal()
        await drain(model)

        XCTAssertFalse(node.isRunning, "the stop was swallowed — the node kept running")
        XCTAssertGreaterThanOrEqual(node.stopCalls, 1)
        // The redundant second stop lands on NotRunning, which is a no-op
        // success — never a user-visible "Stop failed".
        XCTAssertNil(model.lastOutcome)
    }

    /// A start that RETURNS into a backgrounded app stops again rather than
    /// leaving the node running headless (KTD-10), and the missed transition
    /// does not strand the requested flag: the next foreground start works.
    func testStartReturningIntoTheBackgroundStopsAgain() async {
        var backgrounded = false
        let model = WalletModel(isBackgrounded: { backgrounded })
        let node = FakeNode()
        node.gateStart = true

        model.startNode(node, eventSource: ClosedEventSource())
        // The app backgrounds while start() blocks; its scenePhase stop was
        // dropped because no start had landed yet.
        backgrounded = true
        node.startGate.signal()
        await drain(model)

        XCTAssertFalse(
            node.isRunning,
            "a headless node survived the background transition that landed mid-start"
        )

        backgrounded = false
        model.startNode(node, eventSource: ClosedEventSource())
        await drain(model)
        XCTAssertTrue(node.isRunning, "the next foreground start was stranded by a stale flag")
    }

    /// `AlreadyRunning` is a no-op success (Android's `startCore` parity): the
    /// node is up, which is all the start wanted. Treating it as a failure
    /// cleared `startRequested`, which then made the next `stop()` return
    /// early — the running node was stranded with the shell believing it
    /// stopped.
    func testAlreadyRunningStartIsANoOpSuccess() async {
        let model = WalletModel(isBackgrounded: { false })
        // The node is still up — e.g. a foreground flip landing while the
        // previous stop is still draining behind the core's state mutex.
        let node = FakeNode(running: true)

        model.startNode(node, eventSource: ClosedEventSource())
        await drain(model)

        XCTAssertNil(model.startError, "AlreadyRunning was surfaced as a start failure")
        XCTAssertNil(model.lastOutcome)
        XCTAssertTrue(node.isRunning)

        // The benign race did not clear the requested state: the next stop is
        // still honoured.
        model.stopNode(node)
        await drain(model)
        XCTAssertFalse(node.isRunning, "the stop after an AlreadyRunning start was swallowed")
        XCTAssertEqual(1, node.stopCalls)
    }

    /// A genuinely failing start still surfaces on Home and releases the
    /// requested state so a later start can retry.
    func testFailingStartStillSurfacesAndReleasesTheRequestedState() async {
        let model = WalletModel(isBackgrounded: { false })
        let node = FailingNode()

        model.startNode(node, eventSource: ClosedEventSource())
        await drain(model)

        XCTAssertNotNil(model.startError)
        // Not stuck: the flag was released, so the next start is attempted.
        model.startNode(node, eventSource: ClosedEventSource())
        await drain(model)
        XCTAssertEqual(2, node.startCalls)
    }

    /// A node whose start always fails with an untyped error.
    private final class FailingNode: WalletLifecycle, @unchecked Sendable {
        private let lock = NSLock()
        private var starts = 0

        var startCalls: Int { lock.withLock { starts } }

        func startNode() throws {
            lock.withLock { starts += 1 }
            throw NSError(domain: "test", code: 1, userInfo: [
                NSLocalizedDescriptionKey: "esplora unreachable",
            ])
        }

        func stopNode() throws {}
    }
}
