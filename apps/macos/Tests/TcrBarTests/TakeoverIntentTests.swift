import XCTest

@testable import TcrBarCore

/// The same stderr means opposite things depending on which arguments produced it.
///
/// This is a regression test for a shipped bug. `classifyExit` was argument-blind:
/// it matched an incumbent marker and returned the benign `incumbentHoldsPort` no
/// matter why the spawn was made. So clicking "Take over port" — which runs
/// `tcr server --replace` — produced this sequence:
///
///  1. `tcr`'s port singleton declined to replace the incumbent, because that
///     incumbent was a proxy hosted inside a `tcr run` process and
///     `is_proxy_server` deliberately excludes those (`src/singleton.rs:38,62`,
///     asserted at `:257`). Killing one would kill the Claude session running
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
            return XCTFail("a start without --replace finding an incumbent is expected, got \(state)")
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

    /// Verbatim clap 4 output when an argument the binary does not define is
    /// passed — what a `tcr` predating the `--replace` flip answers the takeover
    /// with, on exit code 2.
    private let unknownArgument = """
        error: unexpected argument '--replace' found

          tip: to pass '--replace' as a value, use '-- --replace'

        Usage: tcr server [OPTIONS]

        For more information, try '--help'.
        """

    func testAnOldBinaryRejectingTheFlagIsReportedAsAnOutdatedTool() {
        let state = ServerController.classifyExit(
            intent: .takeover, exitCode: 2, stderr: unknownArgument
        )
        guard case .toolTooOld = state else {
            return XCTFail("a clap usage error about --replace means the CLI is stale, got \(state)")
        }
        let summary = state.summary.lowercased()
        XCTAssertTrue(summary.contains("too old"), "name the cause: \(state.summary)")
        XCTAssertTrue(
            summary.contains("update"),
            "say what to do about it: \(state.summary)"
        )
    }

    /// The failure this classification exists to prevent: an opaque
    /// `Server exited (2): error: unexpected argument …` shown to someone who has
    /// just confirmed a destructive alert.
    func testTheStaleBinaryIsNotFiledAsAnOpaqueExit() {
        let state = ServerController.classifyExit(
            intent: .takeover, exitCode: 2, stderr: unknownArgument
        )
        if case .exited = state { XCTFail("a usage error is actionable, not an opaque exit") }
        if case .incumbentHoldsPort = state { XCTFail("nothing was taken over and nothing is ours") }
    }

    /// Only `--replace` is version-gated. A usage error about any other argument
    /// is a bug in this app, not a stale CLI, and must not be dressed up as one.
    func testAUsageErrorAboutSomeOtherArgumentIsStillAPlainExit() {
        let state = ServerController.classifyExit(
            intent: .takeover,
            exitCode: 2,
            stderr: "error: unexpected argument '--frobnicate' found"
        )
        guard case .exited(let code, _) = state else {
            return XCTFail("expected a plain exit, got \(state)")
        }
        XCTAssertEqual(code, 2)
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

/// The takeover button has to work against whatever `tcr` is installed, which is
/// not necessarily the one this app shipped beside: TcrBar and the CLI are
/// separate installs, and the flag that means "take the port" changed spelling.
///
/// Against a binary predating that change, `--replace` does not exist and clap
/// rejects it outright — so the button that used to work became a usage error,
/// and the version skew `safeArguments` documents itself as defending against was
/// only defended on the safe path.
final class ReplaceFlagCapabilityTests: XCTestCase {

    /// Shape of `tcr server --help` on a build that has the flag. Both spellings
    /// appear, because `--no-replace` is still accepted (`src/main.rs:187-192`).
    private let modernHelp = """
        Usage: tcr server [OPTIONS]

        Options:
              --port <PORT>      Port to bind (overrides `proxy.port` from the config)
              --headless         Run without the TUI, logging to stdout
              --replace          Take over the port: kill a proxy already listening on it
              --no-replace       DEPRECATED and now a no-op: this is the default
          -h, --help             Print help
        """

    /// The same screen on a build that predates the flip: `--no-replace` is the
    /// only spelling, and taking the port is what happens by default.
    private let legacyHelp = """
        Usage: tcr server [OPTIONS]

        Options:
              --port <PORT>      Port to bind (overrides `proxy.port` from the config)
              --headless         Run without the TUI, logging to stdout
              --no-replace       Refuse the port if another proxy already holds it
          -h, --help             Print help
        """

    /// A build whose help offers only `--no-replace` is an old one.
    ///
    /// Note for anyone tempted by the obvious shortcut: `"--no-replace"` does not
    /// contain `"--replace"` (one hyphen before `replace`, not two), so this case
    /// alone does not distinguish substring matching from token matching. The next
    /// test is the one that does.
    func testAnOlderHelpScreenIsNotReadAsSupportingTheFlag() {
        XCTAssertEqual(
            ServerController.replaceFlagSupport(inHelpText: legacyHelp),
            .unsupported
        )
    }

    /// The trap that is real: a *longer* flag starting with the same characters.
    /// `text.contains("--replace")` answers "supported" for a binary that has no
    /// `--replace` at all, and the takeover then dies on the usage error this
    /// probe exists to avoid. Whole-token matching is what prevents it, and this
    /// is the test that fails if anyone simplifies it back.
    func testAFlagThatMerelyBeginsWithReplaceDoesNotCount() {
        let help = """
            Usage: tcr server [OPTIONS]

            Options:
                  --headless             Run without the TUI, logging to stdout
                  --replace-if-stale     Take the port only from an outdated proxy
                  --no-replace           Refuse the port if another proxy holds it
            """
        XCTAssertEqual(
            ServerController.replaceFlagSupport(inHelpText: help),
            .unsupported,
            "`--replace-if-stale` is not `--replace` — matching must be on whole tokens"
        )
    }

    func testACurrentHelpScreenReportsTheFlag() {
        XCTAssertEqual(ServerController.replaceFlagSupport(inHelpText: modernHelp), .supported)
    }

    /// No evidence either way is treated as modern on purpose. That path ends in a
    /// usage error this app explains; guessing "old" against a current binary
    /// would instead send arguments that make it stand down, so the operator would
    /// be told the takeover was refused by a proxy that was never asked.
    func testUnreadableHelpAssumesTheCurrentFlag() {
        for text in ["", "tcr: command not found", "Usage: tcr server [OPTIONS]"] {
            XCTAssertEqual(ServerController.replaceFlagSupport(inHelpText: text), .supported)
        }
    }

    func testEachVintageGetsTheArgumentThatActuallyTakesThePort() {
        let modern = ServerController.takeoverArgumentSet(.supported)
        XCTAssertTrue(modern.contains("--replace"), "\(modern) asks a current tcr for nothing")

        // On an old binary taking over WAS the default, and `--no-replace` was the
        // only way to decline it — so the takeover is expressed by passing neither.
        let legacy = ServerController.takeoverArgumentSet(.unsupported)
        XCTAssertFalse(
            legacy.contains("--replace"),
            "\(legacy) is rejected by the binary it exists for"
        )
        XCTAssertFalse(
            legacy.contains("--no-replace"),
            "\(legacy) tells an old tcr to do the opposite of what was asked"
        )
    }

    func testBothVintagesStayHeadless() {
        for arguments in [
            ServerController.takeoverArgumentSet(.supported),
            ServerController.takeoverArgumentSet(.unsupported),
        ] {
            XCTAssertEqual(arguments.first, "server")
            XCTAssertTrue(
                arguments.contains("--headless"),
                "\(arguments) would start a TUI with no terminal and die on launch"
            )
        }
    }

    /// A probe that never answers must not disable the button forever. Before the
    /// deadline, one unanswerable `--help` left `probing` true for the lifetime of
    /// the app and every later click returned at the guard, in silence.
    func testAProbeThatNeverAnswersFallsBackInsteadOfHanging() async {
        let started = Date()
        let support = await ServerController.support(within: 0.2) {
            // Blocking and cancellation-deaf, exactly like the subprocess read it
            // stands in for. A task-group race cannot abandon this.
            Thread.sleep(forTimeInterval: 5)
            return .unsupported
        }
        let elapsed = Date().timeIntervalSince(started)
        XCTAssertEqual(support, .supported, "a timed-out probe assumes the current flag")
        XCTAssertLessThan(
            elapsed, 2,
            "the deadline did not fire after \(elapsed)s — the takeover would hang"
        )
    }

    /// And a probe that does answer is still the one that decides.
    func testAnAnsweringProbeIsNotOverriddenByTheDeadline() async {
        let support = await ServerController.support(within: 30) { .unsupported }
        XCTAssertEqual(support, .unsupported)
    }

    /// Neither vintage may ever name a pid or a signal: the replacement happens
    /// inside `tcr`, never here.
    func testNeitherVintageNamesAProcess() {
        for arguments in [
            ServerController.takeoverArgumentSet(.supported),
            ServerController.takeoverArgumentSet(.unsupported),
        ] {
            XCTAssertFalse(arguments.contains { $0.contains("kill") || $0.contains("pid") })
        }
    }
}

/// `tcr` now stands down with exit code **0**, so stderr is the only thing that
/// says an incumbent is there. Losing it is no longer a cosmetic loss: the panel
/// renders "Server exited (0): no output" for a proxy that is still serving.
///
/// Measured before the fix, 400 spawns of a child writing 600 bytes and exiting
/// under load: 10 snapshots came back empty or truncated (second run: 9). After
/// it, 0 of 400 twice.
final class ChildStderrTests: XCTestCase {

    /// The race, made deterministic. `installReadabilityHandler: false` is exactly
    /// the state the race produces by accident — bytes sitting in the pipe that
    /// the streaming callback never got to. If `finish()` snapshots instead of
    /// draining, this fails every single run.
    func testFinishDrainsWhatTheStreamingReaderNeverSaw() throws {
        let pipe = Pipe()
        let stderr = ChildStderr(reading: pipe, installReadabilityHandler: false)
        let message = "[tcr] another proxy holds :3456 (pid 123) and it is still listening"
        try pipe.fileHandleForWriting.write(contentsOf: Data(message.utf8))
        try pipe.fileHandleForWriting.close()

        XCTAssertEqual(stderr.finish(), message, "the final stderr chunk was dropped")
    }

    /// And the consequence, spelled out: a dropped stand-down reads as a clean
    /// exit, which is the app reporting success while an incumbent still serves.
    func testADroppedStandDownWouldReadAsACleanExit() throws {
        let pipe = Pipe()
        let stderr = ChildStderr(reading: pipe, installReadabilityHandler: false)
        let standDown = "[tcr] another proxy holds :3456 (pid 123) and it is still listening — "
            + "leaving it alone and exiting without binding."
        try pipe.fileHandleForWriting.write(contentsOf: Data(standDown.utf8))
        try pipe.fileHandleForWriting.close()

        let state = ServerController.classifyExit(
            intent: .safeStart, exitCode: 0, stderr: stderr.finish()
        )
        guard case .incumbentHoldsPort = state else {
            return XCTFail("a stand-down whose stderr was dropped reports success: \(state)")
        }
    }

    /// The streaming path still works, and draining afterwards must not duplicate
    /// what it already read — a read consumes its bytes, so each arrives once.
    func testStreamedOutputIsCollectedExactlyOnce() throws {
        let pipe = Pipe()
        let stderr = ChildStderr(reading: pipe)
        let chunk = "half"
        try pipe.fileHandleForWriting.write(contentsOf: Data(chunk.utf8))
        // Give the readability callback a chance to run before EOF.
        Thread.sleep(forTimeInterval: 0.2)
        try pipe.fileHandleForWriting.close()

        let collected = stderr.finish()
        XCTAssertEqual(
            collected.components(separatedBy: chunk).count - 1,
            1,
            "stderr was collected twice: \(collected)"
        )
    }

    /// End to end against a real child that writes and exits in the same breath —
    /// the shape that produced the race.
    func testARealChildsFinalChunkSurvivesItsExit() throws {
        let payload = String(repeating: "x", count: 600)
        for _ in 0..<20 {
            let process = Process()
            process.executableURL = URL(fileURLWithPath: "/bin/sh")
            process.arguments = ["-c", "printf '%s' '\(payload)' >&2"]
            let pipe = Pipe()
            process.standardError = pipe
            process.standardOutput = FileHandle.nullDevice
            let stderr = ChildStderr(reading: pipe)
            let finished = expectation(description: "child exited")
            let collected = LockedString()
            process.terminationHandler = { _ in
                collected.append(stderr.finish())
                finished.fulfill()
            }
            try process.run()
            wait(for: [finished], timeout: 10)
            XCTAssertEqual(collected.value.count, payload.count, "stderr was lost or truncated")
        }
    }
}
