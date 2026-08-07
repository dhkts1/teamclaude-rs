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
    ///
    /// Measured, not recalled: produced by compiling this project's own
    /// `ServerArgs` wiring minus `--replace` against clap 4 and parsing
    /// `["server", "--headless", "--replace"]`. The `tip:` line in particular is
    /// not what one would guess — clap suggests the *similar* argument.
    private let unknownArgument = """
        error: unexpected argument '--replace' found

          tip: a similar argument exists: '--no-replace'

        Usage: tcr server --headless --no-replace

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

/// A stand-down is no longer one outcome with one exit code.
///
/// `tcr` now distinguishes three (`src/main.rs:479-494`): 0 when it stood down
/// and the incumbent answered, 3 when the incumbent is serving a stale build, and
/// 4 when the incumbent answered *nothing* — it holds the listening socket and
/// serves no requests.
///
/// All three print the same `another proxy holds` line, so classification on the
/// marker alone collapses them into the benign "already running". For exit 4 that
/// is the inverse of the truth, and it is the worst thing this panel can do: tell
/// the operator the proxy is fine while every session through the port fails.
final class StandDownExitCodeTests: XCTestCase {

    /// Verbatim from `singleton::stand_down_message` — the line every stand-down
    /// prints, regardless of which of the three it is.
    private let standDown = "[tcr] another proxy holds :3456 (pid 123) and it is still "
        + "listening — leaving it alone and exiting without binding. Replacing it would wipe "
        + "its session→account pin map and cold-start every live session's prompt cache, the "
        + "most expensive event in this system. Pass --replace to take the port over anyway."

    /// Verbatim from the `Liveness::Silent` arm at `src/main.rs:559-565`.
    private var silentIncumbent: String {
        standDown + "\n"
            + "[tcr] WARNING incumbent-not-answering: port=3456 pid=123 probe=\"timeout\" — the "
            + "process holding :3456 did not respond, so standing down leaves NOTHING serving "
            + "on it. Run `tcr --replace` to take the port over; that is the recovery for a "
            + "wedged proxy, and it is not being done automatically because it also wipes the "
            + "pin map of a proxy that was merely slow to answer."
    }

    private var staleIncumbent: String {
        standDown + "\n"
            + "[tcr] WARNING stale-server: running=abc1234 this_binary=def5678 built_dirty=false "
            + "this_binary_dirty=false — the proxy on :3456 is running a DIFFERENT commit."
    }

    /// The other half of a cross-language contract no compiler checks.
    ///
    /// These numbers are `EXIT_STOOD_DOWN_STALE` and
    /// `EXIT_STOOD_DOWN_NOT_ANSWERING` in `src/main.rs`, and Rust's
    /// `the_stand_down_exit_codes_are_the_numbers_tcrbar_switches_on` transcribes
    /// them in the opposite direction. Both copies are needed, because each test
    /// only catches a renumbering that starts on the *other* side: theirs pins
    /// Rust against a copy of these values, so a change made here would leave it
    /// green, and this one closes that direction.
    ///
    /// Renumbering is a one-character edit that every other test in either suite
    /// survives, and the consequence is silent — TcrBar falls through to a bare
    /// `.exited(5, …)` and reports a proxy serving NOTHING as a clean exit, which
    /// is the misreport this whole round exists to eliminate.
    ///
    /// The numbers are SPELLED OUT rather than referenced through the constants.
    /// `XCTAssertEqual(StandDownExit.stale, StandDownExit.stale)` compares a value
    /// with itself and passes for every value of it — the constant is exactly the
    /// thing that must not drift, so the test has to hold the other copy. This is
    /// not hypothetical: a mutation earlier in this branch survived because an
    /// assertion was written against the implementation instead of against an
    /// independent statement of the expected value.
    func testTheStandDownExitCodesAreTheNumbersTcrIsAsserting() {
        XCTAssertEqual(
            ServerController.StandDownExit.stale, 3,
            "EXIT_STOOD_DOWN_STALE is 3 in src/main.rs — change one, change both"
        )
        XCTAssertEqual(
            ServerController.StandDownExit.notAnswering, 4,
            "EXIT_STOOD_DOWN_NOT_ANSWERING is 4 in src/main.rs — change one, change both"
        )
    }

    /// The constants being right is worthless if the classifier does not act on
    /// them, so the contract is asserted a second time through the function the
    /// panel's text actually comes from, against the same literals rather than
    /// through the constants.
    func testTheLiteralCodesReachTheStatesTheyAreDefinedFor() {
        guard case .incumbentIsStale = ServerController.classifyExit(
            intent: .safeStart, exitCode: 3, stderr: standDown
        ) else {
            return XCTFail("literal 3 must classify as a stale incumbent")
        }
        guard case .incumbentNotAnswering = ServerController.classifyExit(
            intent: .safeStart, exitCode: 4, stderr: standDown
        ) else {
            return XCTFail("literal 4 must classify as a wedged incumbent")
        }
    }

    func testASilentIncumbentIsNotReportedAsAlreadyRunning() {
        let state = ServerController.classifyExit(
            intent: .safeStart, exitCode: 4, stderr: silentIncumbent
        )
        if case .incumbentHoldsPort = state {
            XCTFail("a process serving nothing was reported as a healthy incumbent")
        }
        guard case .incumbentNotAnswering = state else {
            return XCTFail("expected the not-answering state, got \(state)")
        }
    }

    /// The panel must not claim service that is not happening, and must name the
    /// recovery without performing it — the Rust side deliberately refuses to
    /// auto-takeover on this evidence, and this app must not re-introduce that
    /// decision quietly.
    func testTheSilentIncumbentSummaryTellsTheTruthAndNamesTheRecovery() {
        let summary = ServerController.classifyExit(
            intent: .safeStart, exitCode: 4, stderr: silentIncumbent
        ).summary

        XCTAssertTrue(
            summary.contains("NOT SERVING"),
            "the headline fact is that nothing is being served: \(summary)"
        )
        XCTAssertFalse(
            summary.contains("Already running — not ours, left alone."),
            "that is the benign wording, and it is false here: \(summary)"
        )
        XCTAssertTrue(
            summary.contains("Take over port"),
            "name the recovery: \(summary)"
        )
        XCTAssertTrue(
            summary.contains("will not make that call for you"),
            "say that the takeover is the operator's decision, not ours: \(summary)"
        )
    }

    /// On the takeover path the same code is still the more useful fact.
    /// `.takeoverRefused` says "the existing proxy is still serving", which for
    /// exit 4 would be a second lie on top of the first.
    func testASilentIncumbentIsNotDressedUpAsAMerelyRefusedTakeover() {
        let state = ServerController.classifyExit(
            intent: .takeover, exitCode: 4, stderr: silentIncumbent
        )
        guard case .incumbentNotAnswering = state else {
            return XCTFail("expected the not-answering state, got \(state)")
        }
        XCTAssertFalse(
            state.summary.contains("the existing proxy is still serving"),
            "it is not serving: \(state.summary)"
        )
    }

    /// The exit code carries the verdict even when stderr does not arrive at all.
    /// That is not hypothetical — the pipe race this file also tests could drop
    /// the whole message, and a lost stand-down used to read as a clean exit.
    func testTheVerdictSurvivesAnEmptyStderr() {
        guard case .incumbentNotAnswering = ServerController.classifyExit(
            intent: .safeStart, exitCode: 4, stderr: ""
        ) else {
            return XCTFail("the exit code alone must carry a wedged incumbent")
        }
        guard case .incumbentIsStale = ServerController.classifyExit(
            intent: .safeStart, exitCode: 3, stderr: ""
        ) else {
            return XCTFail("the exit code alone must carry a stale incumbent")
        }
    }

    func testAStaleIncumbentIsDistinguishableButNotAnAlarm() {
        let state = ServerController.classifyExit(
            intent: .safeStart, exitCode: 3, stderr: staleIncumbent
        )
        guard case .incumbentIsStale = state else {
            return XCTFail("expected the stale state, got \(state)")
        }
        // It IS serving, so it must not borrow the not-answering alarm.
        XCTAssertFalse(state.summary.contains("NOT SERVING"), state.summary)
        XCTAssertTrue(
            state.summary.contains("older build"),
            "say what is actually wrong with it: \(state.summary)"
        )
        XCTAssertFalse(state.isOurChild)
    }

    /// Exit 0 is unchanged: a stand-down against an answering incumbent is the
    /// benign outcome on a safe start and a failure on a takeover, exactly as
    /// before the exit codes existed.
    func testAnAnsweringIncumbentKeepsTheOldBehaviourOnBothPaths() {
        guard case .incumbentHoldsPort = ServerController.classifyExit(
            intent: .safeStart, exitCode: 0, stderr: standDown
        ) else {
            return XCTFail("exit 0 on a safe start is still the benign outcome")
        }
        guard case .takeoverRefused = ServerController.classifyExit(
            intent: .takeover, exitCode: 0, stderr: standDown
        ) else {
            return XCTFail("exit 0 on a takeover is still a failure")
        }
    }

    /// All three stand-downs print the same marker line, which is precisely why
    /// the code has to be read: on the marker alone these are one state.
    func testTheThreeStandDownsAreNotDistinguishableByTheirMarker() {
        for text in [standDown, silentIncumbent, staleIncumbent] {
            XCTAssertTrue(
                text.contains("another proxy holds"),
                "the marker is common to all three — the exit code is the only separator"
            )
        }
        let states = [Int32(0), 3, 4].map {
            ServerController.classifyExit(intent: .safeStart, exitCode: $0, stderr: standDown)
        }
        XCTAssertEqual(Set(states.map(\.summary)).count, 3, "identical stderr, three verdicts")
    }
}

/// `--replace` together with `--no-replace` is now a hard clap conflict
/// (`conflicts_with`), and it exits 2 — the same code as an unknown argument.
///
/// The two must not be confused. Telling an operator their `tcr` is too old and
/// to go rebuild it, when the binary is current and the real fault is a
/// contradictory flag pair, is confidently wrong in the direction that wastes the
/// most of their time.
final class FlagConflictTests: XCTestCase {

    /// Measured against clap 4 with this project's own `ServerArgs` wiring,
    /// including `#[arg(long, conflicts_with = "replace")]`.
    private let flagConflict = """
        error: the argument '--replace' cannot be used with '--no-replace'

        Usage: tcr server --headless --replace

        For more information, try '--help'.
        """

    func testAFlagConflictIsNotReportedAsAnOutdatedTool() {
        let state = ServerController.classifyExit(
            intent: .takeover, exitCode: 2, stderr: flagConflict
        )
        if case .toolTooOld = state {
            XCTFail("a current binary would be sent to be rebuilt for no reason: \(state.summary)")
        }
        guard case .exited(let code, let message) = state else {
            return XCTFail("a flag conflict is a bug in this app, reported verbatim, got \(state)")
        }
        XCTAssertEqual(code, 2)
        XCTAssertTrue(message.contains("cannot be used with"))
    }

    /// The reason the collision cannot happen in the field, asserted rather than
    /// assumed: no argument set this app can spawn carries both flags. Checked
    /// across every set, not just the takeover one.
    func testNoArgumentSetThisAppCanSpawnCarriesBothFlags() {
        for arguments in [
            ServerController.safeArguments,
            ServerController.serverArguments,
            ServerController.takeoverArguments,
            ServerController.legacyTakeoverArguments,
            ServerController.takeoverArgumentSet(.supported),
            ServerController.takeoverArgumentSet(.unsupported),
        ] {
            XCTAssertFalse(
                arguments.contains("--replace") && arguments.contains("--no-replace"),
                "\(arguments) is now rejected by clap outright — tcr exits 2 and nothing starts"
            )
        }
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
