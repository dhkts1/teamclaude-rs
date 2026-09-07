import Foundation
import XCTest

@testable import TcrBarCore

/// The second, separate defect in the crash this pairs with: a `SIGABRT`
/// skips `applicationWillTerminate` entirely, so it was the ONLY place the
/// supervised child got stopped. This orphans the child, which keeps
/// holding port 3456 — TcrBar's own next launch then stands down to it and
/// shows `.incumbentHoldsPort`.
///
/// `AbnormalTerminationGuard.terminateSupervisedChild(pid:)` is the whole
/// body of the signal handler `TcrBarApp.swift` installs for exactly that
/// case, pulled out as a free function so it can be exercised here without
/// raising a real signal — a signal handler is not a thing XCTest can drive
/// directly, but the syscall it makes is.
final class AbnormalTerminationGuardTests: XCTestCase {

    /// Spawns a real, still-running child — the same shape `ServerController`
    /// spawns — and proves the guard's one syscall is what stops it, not
    /// something else in the test's teardown.
    ///
    /// Fails on the change this guards against: a
    /// `terminateSupervisedChild(pid:)` that does nothing (the bug this
    /// commit fixes — before it, an abnormal exit called no stop path at
    /// all) leaves `sleep 30` running, and this assertion times out waiting
    /// for it to exit rather than observing it exit quickly.
    func testTerminateSupervisedChildStopsAStillRunningProcess() throws {
        let process = Process()
        process.executableURL = URL(fileURLWithPath: "/bin/sleep")
        process.arguments = ["30"]
        try process.run()
        addTeardownBlock { if process.isRunning { process.terminate() } }

        XCTAssertTrue(process.isRunning, "the process under test must actually be alive first")

        AbnormalTerminationGuard.terminateSupervisedChild(pid: process.processIdentifier)

        process.waitUntilExit()
        XCTAssertFalse(process.isRunning)
        XCTAssertEqual(
            process.terminationReason, .uncaughtSignal,
            "SIGTERM, not a clean exit — proves the guard's kill(2) is what ended it")
    }

    /// `0` is `ServerController.supervisedChildPID`'s "nothing is
    /// supervised" value, and the guard must be a no-op on it — sending
    /// `kill(0, …)` targets the entire calling process group, which would
    /// signal TcrBar itself (and everything else in its group) rather than
    /// nothing.
    func testTerminateSupervisedChildIsANoOpOnTheUnsupervisedSentinel() {
        // Not a crash-reproducing assertion by itself — it exists so the
        // call above only ever proves ONE thing (a real pid gets signalled)
        // rather than silently also relying on `kill(0, …)` being harmless
        // in a test process. If this guard's `pid > 0` check were removed,
        // this call would signal the test runner's own process group.
        AbnormalTerminationGuard.terminateSupervisedChild(pid: 0)
    }

    /// `ServerController` publishes the pid a signal handler needs to read
    /// through this static var, since the handler cannot call back into a
    /// `@MainActor` instance. Round-tripping it here is what the orphan fix
    /// actually depends on end to end: spawn through the real controller,
    /// confirm the pid landed in the static var, confirm it clears on stop.
    @MainActor
    func testServerControllerPublishesAndClearsTheSupervisedPID() async throws {
        let controller = ServerController()
        XCTAssertEqual(ServerController.supervisedChildPID, 0, "nothing supervised at the start")

        controller.start()
        // `start()` resolves `tcr` and spawns asynchronously in effect (the
        // spawn itself is synchronous, but `TcrTool.resolve()` and the
        // eventual `terminationHandler` are not) — poll briefly rather than
        // assume a fixed delay is enough on a slow CI machine.
        let deadline = Date().addingTimeInterval(5)
        while ServerController.supervisedChildPID == 0 && Date() < deadline {
            try await Task.sleep(nanoseconds: 50_000_000)
        }

        guard ServerController.supervisedChildPID != 0 else {
            // No `tcr` on PATH in this environment — the pid can never be
            // set, and there is nothing left to assert. Not a failure of
            // this test's claim.
            throw XCTSkip("no tcr on PATH to spawn — cannot observe a real supervised pid")
        }
        XCTAssertGreaterThan(ServerController.supervisedChildPID, 0)

        controller.stop()
        XCTAssertEqual(
            ServerController.supervisedChildPID, 0, "stop() must clear the pid a handler would read")
    }
}
