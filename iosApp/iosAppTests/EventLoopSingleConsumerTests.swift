import XCTest

@testable import iosApp

/// A fake event queue standing in for the wallet's persisted handle-then-ack
/// queue. It counts how many consumers sit inside `nextWalletEvent` at once —
/// the queue's contract is exactly one — and holds each consumer briefly so
/// an overlapping second consumer is observable, which is precisely how the
/// old cancel-and-restart loop double-consumed (the cancelled task could be
/// past its cancellation check while the replacement already awaited next).
actor FakeEventQueue: WalletEventSource {
    private var pending: [WalletEvent] = []
    private var waiter: CheckedContinuation<WalletEvent, Never>?
    private var consumers = 0
    private(set) var maxConcurrentConsumers = 0
    private(set) var consumedCount = 0
    private(set) var ackedCount = 0

    func nextWalletEvent() async throws -> WalletEvent {
        consumers += 1
        maxConcurrentConsumers = max(maxConcurrentConsumers, consumers)
        // Hold the consumer inside next long enough for a would-be second
        // consumer to overlap (a suspension point releases the actor).
        try? await Task.sleep(nanoseconds: 20_000_000)
        let event: WalletEvent
        if pending.isEmpty {
            event = await withCheckedContinuation { waiter = $0 }
        } else {
            event = pending.removeFirst()
        }
        consumedCount += 1
        consumers -= 1
        return event
    }

    func ackEvent() async throws {
        ackedCount += 1
    }

    func push(_ event: WalletEvent) {
        if let waiter {
            self.waiter = nil
            waiter.resume(returning: event)
        } else {
            pending.append(event)
        }
    }
}

/// Regression test for the U19 P2 fix: rapid background→foreground flips
/// (stop pushes the terminal NodeStopped; start reschedules the loop) must
/// never leave two loops consuming the queue concurrently. Under the old
/// cancel-and-restart scheduling this recorded `maxConcurrentConsumers == 2`;
/// the chained single-consumer scheduling keeps it at 1 while still
/// consuming and acking every event exactly once.
final class EventLoopSingleConsumerTests: XCTestCase {
    @MainActor
    func testRapidStopStartCyclesNeverRunTwoLoopsConcurrently() async {
        let model = WalletModel()
        let queue = FakeEventQueue()

        // Foreground: first start schedules the loop, which parks inside next.
        model.scheduleEventLoop(queue)
        await queue.push(.syncCompleted)

        // Rapid flip #1: stop() pushes the terminal NodeStopped while the
        // loop is still busy with the previous event, and the immediate
        // restart schedules the next run BEFORE the old one has drained.
        model.scheduleEventLoop(queue)
        await queue.push(.nodeStopped)

        // Rapid flip #2: same race again, plus a live event for the new run.
        model.scheduleEventLoop(queue)
        await queue.push(.paymentSuccessful)
        await queue.push(.nodeStopped)

        // Terminal NodeStopped for the last scheduled run so the chain ends.
        await queue.push(.nodeStopped)
        await model.eventLoopTask?.value

        let maxConcurrent = await queue.maxConcurrentConsumers
        XCTAssertEqual(
            1, maxConcurrent,
            "two event-loop runs consumed the queue concurrently — single-consumer contract violated"
        )
        // Every pushed event was consumed exactly once and acked (no
        // double-consumption, no drops across the stop/start flips).
        let consumed = await queue.consumedCount
        let acked = await queue.ackedCount
        XCTAssertEqual(5, consumed)
        XCTAssertEqual(5, acked)
    }

    /// The loop exits on the terminal NodeStopped (pushed by stop()) rather
    /// than relying on cancellation, and a subsequent schedule starts a fresh
    /// run that keeps consuming.
    @MainActor
    func testLoopExitsOnNodeStoppedAndRestartsCleanly() async {
        let model = WalletModel()
        let queue = FakeEventQueue()

        model.scheduleEventLoop(queue)
        await queue.push(.nodeStopped)
        await model.eventLoopTask?.value
        let afterFirst = await queue.consumedCount
        XCTAssertEqual(1, afterFirst)

        model.scheduleEventLoop(queue)
        await queue.push(.channelReady)
        await queue.push(.nodeStopped)
        await model.eventLoopTask?.value

        let consumed = await queue.consumedCount
        let maxConcurrent = await queue.maxConcurrentConsumers
        XCTAssertEqual(3, consumed)
        XCTAssertEqual(1, maxConcurrent)
    }
}
