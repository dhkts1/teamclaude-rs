import XCTest

@testable import TcrBarCore

/// The same stderr means opposite things depending on which arguments produced it.
///
/// This is a regression test for a shipped bug. `classifyExit` was argument-blind:
/// it matched an incumbent marker and returned the benign `incumbentHoldsPort` no
/// matter why the spawn was made. So clicking "Take over port" — which runs
/// `tcr server` *without* `--no-replace` — produced this sequence:
///
///  1. `tcr`'s port singleton declined to replace the incumbent, because that
///     incumbent was a proxy hosted inside a `tcr run` process and
///     `is_proxy_server` deliberately excludes those (`src/singleton.rs:27,52`,
///     asserted at `:192`). Killing one would kill the Claude session running
///     through it.
///  2. The subsequent bind failed: `failed to bind 127.0.0.1:3456`.
///  3. `classifyExit` matched `failed to bind`, reported "already running", and
///     the panel showed the benign outcome.
///
/// The user asked to take over. Nothing was taken over. The app said it was fine.
final class TakeoverIntentTests: XCTestCase {

    /// Verbatim shape of what `tcr` emits on a refused bind. Confirmed by compiling
    /// a probe against a real `EADDRINUSE` (errno 48) rather than assuming how
    /// anyhow renders a context line.
    private let refusedBind = """
        Error: failed to bind 127.0.0.1:3456

        Caused by:
            Address already in use (os error 48)
        """

    func testSafeStartTreatsAnIncumbentAsTheBenignOutcome() {
        let state = ServerController.classifyExit(
            intent: .safeStart, exitCode: 1, stderr: refusedBind
        )
        guard case .incumbentHoldsPort = state else {
            return XCTFail("a --no-replace start finding an incumbent is expected, got \(state)")
        }
    }

    func testTakeoverTreatsTheSameIncumbentAsAFailure() {
        let state = ServerController.classifyExit(
            intent: .takeover, exitCode: 1, stderr: refusedBind
        )
        guard case .takeoverRefused = state else {
            return XCTFail("a takeover that left the incumbent running has failed, got \(state)")
        }
    }

    /// The precise shape of the shipped bug, pinned so it cannot come back.
    func testTakeoverNeverReportsTheBenignAlreadyRunningState() {
        let state = ServerController.classifyExit(
            intent: .takeover, exitCode: 1, stderr: refusedBind
        )
        if case .incumbentHoldsPort = state {
            XCTFail("this is the shipped bug: a failed takeover reported as success")
        }
    }

    /// The failure has to be legible, not just correctly typed. It is not transient,
    /// so the text must not invite a retry that cannot work.
    func testTheRefusalExplainsWhyRetryingWillNotHelp() {
        let state = ServerController.classifyExit(
            intent: .takeover, exitCode: 1, stderr: refusedBind
        )
        let summary = state.summary

        XCTAssertTrue(summary.contains("tcr run"), "name the cause: \(summary)")
        XCTAssertTrue(
            summary.lowercased().contains("not change the outcome")
                || summary.lowercased().contains("will not"),
            "say plainly that retrying is pointless: \(summary)"
        )
    }

    /// An unrelated failure must still surface verbatim rather than being folded
    /// into either incumbent case.
    func testAnUnrelatedFailureIsStillReportedAsAnExit() {
        let state = ServerController.classifyExit(
            intent: .takeover, exitCode: 101, stderr: "config parse error"
        )
        guard case .exited(let code, let message) = state else {
            return XCTFail("expected a plain exit, got \(state)")
        }
        XCTAssertEqual(code, 101)
        XCTAssertEqual(message, "config parse error")
    }
}
