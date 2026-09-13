import XCTest

@testable import TcrBarCore

/// The Tools tab's process half: which process a running Bash call is, what
/// its tree costs, what the row prints, and the four cases a kill refuses.
///
/// Every test here runs against a ``ProcessSnapshot`` table the test itself
/// writes. Nothing reads the live process table — that is ``ProcessTable/read()``,
/// which is untested for the same reason ``MachineStats/read()`` is: a test
/// that asserts about this machine's processes asserts about the CI runner.
/// The LIVE half (a real `killpg` ending a real process) is a check run by
/// hand and pasted into the bridge's report, not a test that signals
/// processes on a build box.
final class ProcessStatsTests: XCTestCase {
    /// A fixed instant, so "3 seconds apart" means three seconds and not
    /// whatever the clock did between two lines.
    private let callStart = Date(timeIntervalSince1970: 1_789_300_000)
    private var callStartMs: Int64 { Int64(callStart.timeIntervalSince1970 * 1000) }

    private func process(
        pid: Int32,
        parent: Int32,
        group: Int32? = nil,
        name: String = "bash",
        startOffset: TimeInterval = 0,
        cpuSeconds: Double = 0,
        residentBytes: UInt64 = 0
    ) -> ProcessSnapshot {
        ProcessSnapshot(
            pid: pid,
            parentPid: parent,
            processGroup: group ?? pid,
            name: name,
            startedAt: callStart.addingTimeInterval(startOffset),
            cpuSeconds: cpuSeconds,
            residentBytes: residentBytes)
    }

    // MARK: - The matcher

    /// The case the feature exists for: the session's own shell child,
    /// started when the call was.
    func testMatchesTheShellChildStartedWithTheCall() {
        let shell = process(pid: 48765, parent: 31389, startOffset: 0.4)
        let table = [process(pid: 31389, parent: 1, name: "claude"), shell]
        XCTAssertEqual(
            ProcessMatch.shellChild(ofSessionPid: 31389, startedMs: callStartMs, in: table)?.pid,
            48765)
    }

    /// A shell that started ten seconds off this call is a DIFFERENT call's
    /// shell — the previous one, still running, or the next one. Matching it
    /// would put one call's numbers on another call's row and, worse, aim a
    /// kill at it.
    func testRejectsAShellTenSecondsOff() {
        let table = [process(pid: 48765, parent: 31389, startOffset: 10)]
        XCTAssertNil(
            ProcessMatch.shellChild(ofSessionPid: 31389, startedMs: callStartMs, in: table))
    }

    /// The boundary is inclusive at three seconds, exclusive past it — the
    /// figure `docs/design/tools-tab.md` and the bridge both name.
    func testToleranceBoundaryIsThreeSeconds() {
        let onTime = [process(pid: 1, parent: 31389, startOffset: 3.0)]
        let late = [process(pid: 1, parent: 31389, startOffset: 3.01)]
        XCTAssertNotNil(
            ProcessMatch.shellChild(ofSessionPid: 31389, startedMs: callStartMs, in: onTime))
        XCTAssertNil(
            ProcessMatch.shellChild(ofSessionPid: 31389, startedMs: callStartMs, in: late))
    }

    /// A shell of ANOTHER session, started at the same moment, is the case
    /// that makes "started when the call did" insufficient on its own: two
    /// sessions run Bash calls at the same second all the time.
    func testRejectsAShellWithADifferentParent() {
        let table = [process(pid: 48765, parent: 99999, startOffset: 0.2)]
        XCTAssertNil(
            ProcessMatch.shellChild(ofSessionPid: 31389, startedMs: callStartMs, in: table))
    }

    /// A child that is not a shell at all — the session's own subprocess, a
    /// helper, anything — is never a Bash call's process.
    func testRejectsANonShellChild() {
        let table = [process(pid: 48765, parent: 31389, name: "node", startOffset: 0.2)]
        XCTAssertNil(
            ProcessMatch.shellChild(ofSessionPid: 31389, startedMs: callStartMs, in: table))
    }

    /// Two concurrent Bash calls in one session: the nearer start wins, and
    /// the other call's own row matches the other shell.
    func testPicksTheNearestStartWhenTwoShellsQualify() {
        let table = [
            process(pid: 100, parent: 31389, startOffset: 2.5),
            process(pid: 200, parent: 31389, startOffset: 0.2),
        ]
        XCTAssertEqual(
            ProcessMatch.shellChild(ofSessionPid: 31389, startedMs: callStartMs, in: table)?.pid,
            200)
    }

    // MARK: - The tree

    /// The shell itself uses no CPU worth printing; its children do. The row
    /// prints the whole tree or it prints a number that contradicts `top`.
    func testTreeSumsTheShellAndEveryDescendant() {
        let table = [
            process(pid: 10, parent: 31389, cpuSeconds: 0.2, residentBytes: 1_000),
            process(pid: 11, parent: 10, name: "cargo", cpuSeconds: 1.0, residentBytes: 2_000),
            process(pid: 12, parent: 11, name: "rustc", cpuSeconds: 4.0, residentBytes: 4_000),
            // Another session's process, at the same depth, must not be counted.
            process(pid: 20, parent: 99, name: "rustc", cpuSeconds: 9.0, residentBytes: 8_000),
        ]
        XCTAssertEqual(ProcessTree.cpuSeconds(of: 10, in: table), 5.2, accuracy: 0.000_1)
        XCTAssertEqual(ProcessTree.residentBytes(of: 10, in: table), 7_000)
        XCTAssertEqual(ProcessTree.members(of: 10, in: table).count, 3)
    }

    /// A snapshot read across process churn can name a pid as its own
    /// ancestor's parent. The walk must end anyway — a panel poll that never
    /// returns is worse than a missing figure.
    func testTreeWalkTerminatesOnACycle() {
        let table = [
            process(pid: 10, parent: 11, cpuSeconds: 1),
            process(pid: 11, parent: 10, cpuSeconds: 1),
        ]
        XCTAssertEqual(ProcessTree.members(of: 10, in: table).count, 2)
    }

    // MARK: - The CPU rate

    /// 6.4 CPU-seconds over one wall second is 640%: one core saturated is
    /// 100, and this build states a build across seven cores the way `top`
    /// does rather than capping it at 100.
    func testCpuPercentIsCpuSecondsOverWallSeconds() {
        let previous = RunningCallStats(
            pid: 48765, processGroup: 48765, cpuSeconds: 10, residentBytes: 0, cpuPercent: nil,
            readAt: callStart)
        XCTAssertEqual(
            ProcessStats.cpuPercent(
                previous: previous, pid: 48765, cpuSeconds: 16.4,
                now: callStart.addingTimeInterval(1)) ?? -1,
            640, accuracy: 0.000_1)
    }

    /// The first poll has nothing to divide, and a previous reading of a
    /// DIFFERENT pid is not a previous reading of this tree. Both are `nil`,
    /// never 0: "no rate measured yet" and "using no CPU" are different
    /// claims and the row prints them differently.
    func testCpuPercentIsNilWithNothingToCompareAgainst() {
        XCTAssertNil(
            ProcessStats.cpuPercent(previous: nil, pid: 1, cpuSeconds: 5, now: callStart))
        let otherPid = RunningCallStats(
            pid: 777, processGroup: 777, cpuSeconds: 1, residentBytes: 0, cpuPercent: nil,
            readAt: callStart)
        XCTAssertNil(
            ProcessStats.cpuPercent(
                previous: otherPid, pid: 48765, cpuSeconds: 5,
                now: callStart.addingTimeInterval(1)))
    }

    // MARK: - What the row prints

    func testClausePrintsCpuAndMemory() {
        let stats = RunningCallStats(
            pid: 48765, processGroup: 48765, cpuSeconds: 16.4,
            residentBytes: 2_254_857_830, cpuPercent: 640, readAt: callStart)
        XCTAssertEqual(ProcessStatsLabel.clause(stats), " · 640% cpu · 2.1 GB")
        XCTAssertEqual(ProcessStatsLabel.hover(stats, tool: "Bash"), "pid 48765 · Bash")
    }

    /// The first poll shows memory only — there is no rate yet — and a call
    /// with no matched process shows nothing at all rather than a dash, a
    /// zero, or a `·` with empty space after it.
    func testClauseOmitsWhatWasNotMeasured() {
        let firstPoll = RunningCallStats(
            pid: 48765, processGroup: 48765, cpuSeconds: 16.4,
            residentBytes: 2_254_857_830, cpuPercent: nil, readAt: callStart)
        XCTAssertEqual(ProcessStatsLabel.clause(firstPoll), " · 2.1 GB")
        XCTAssertEqual(ProcessStatsLabel.clause(nil), "")
        XCTAssertNil(ProcessStatsLabel.hover(nil, tool: "Bash"))
    }

    /// The two strings a destructive control is judged on: which command it
    /// names, and whether it says what happens to the session.
    func testKillWordingNamesTheCommandAndTheConsequence() {
        XCTAssertEqual(
            ProcessStatsLabel.killLabel(subject: "cargo test --release"),
            "Kill cargo test --release")
        XCTAssertEqual(
            ProcessStatsLabel.killConfirmation(
                subject: "cargo test --release", session: "teamclaude-rs-c7"),
            "cargo test --release in teamclaude-rs-c7."
                + " The session gets a tool error and continues.")
    }

    // MARK: - One poll, end to end

    func testPollMatchesSumsAndRatesAcrossTwoReadings() {
        let call = RunningCallKey(id: "row-1", sessionPid: 31389, startedMs: callStartMs)
        let first = [
            process(pid: 48765, parent: 31389, startOffset: 0.3, cpuSeconds: 1, residentBytes: 100),
            process(pid: 48766, parent: 48765, name: "cargo", cpuSeconds: 2, residentBytes: 900),
        ]
        let firstReading = ProcessStats.poll(
            calls: [call], table: first, previous: [:], now: callStart)
        XCTAssertEqual(firstReading["row-1"]?.pid, 48765)
        XCTAssertEqual(firstReading["row-1"]?.residentBytes, 1_000)
        XCTAssertNil(firstReading["row-1"]?.cpuPercent)

        let second = [
            process(pid: 48765, parent: 31389, startOffset: 0.3, cpuSeconds: 2, residentBytes: 100),
            process(pid: 48766, parent: 48765, name: "cargo", cpuSeconds: 7, residentBytes: 900),
        ]
        let secondReading = ProcessStats.poll(
            calls: [call], table: second, previous: firstReading,
            now: callStart.addingTimeInterval(2))
        XCTAssertEqual(secondReading["row-1"]?.cpuPercent ?? -1, 300, accuracy: 0.000_1)
    }

    /// The shell exited between polls: the row loses its stats and its kill
    /// button. Carrying the last reading forward would draw live numbers for
    /// a dead process and offer to signal a pid that is now somebody else's.
    func testPollDropsACallWhoseProcessIsGone() {
        let call = RunningCallKey(id: "row-1", sessionPid: 31389, startedMs: callStartMs)
        let previous = [
            "row-1": RunningCallStats(
                pid: 48765, processGroup: 48765, cpuSeconds: 1, residentBytes: 100,
                cpuPercent: nil, readAt: callStart)
        ]
        XCTAssertTrue(
            ProcessStats.poll(calls: [call], table: [], previous: previous, now: callStart).isEmpty)
    }

    // MARK: - The kill's refusals

    /// The one refusal that matters most: a matcher bug that returned the
    /// session's own `claude` process must not become a `killpg` of it.
    /// `docs/design/tools-tab.md` § "Not built, on purpose" — killing a
    /// session is a terminal act this panel does not offer.
    func testKillRefusesTheSessionPid() {
        let session = process(pid: 31389, parent: 1, name: "claude")
        XCTAssertEqual(
            ProcessKill.target(matched: session, sessionPid: 31389),
            .failure(.wouldSignalTheSession))
    }

    /// And refuses a child that shares the session's process GROUP, which is
    /// the same kill by another route: `killpg` would take the session down
    /// with the command.
    func testKillRefusesAProcessGroupTheSessionIsIn() {
        let shell = process(pid: 48765, parent: 31389, group: 31389, startOffset: 0.2)
        XCTAssertEqual(
            ProcessKill.target(matched: shell, sessionPid: 31389),
            .failure(.wouldSignalTheSession))
    }

    /// No match at click time is a refusal, not a fallback to the pid the
    /// last poll saw.
    func testKillRefusesWithNothingMatched() {
        XCTAssertEqual(ProcessKill.target(matched: nil, sessionPid: 31389), .failure(.noMatch))
    }

    /// Group 0 is "everything in the caller's own group" — the panel itself.
    /// Group 1 is launchd's.
    func testKillRefusesGroupZeroAndOne() {
        for group in Int32(0)...Int32(1) {
            let shell = process(pid: 48765, parent: 31389, group: group, startOffset: 0.2)
            XCTAssertEqual(
                ProcessKill.target(matched: shell, sessionPid: 31389),
                .failure(.unsafeProcessGroup), "group \(group)")
        }
    }

    func testKillTargetsTheShellsOwnProcessGroup() {
        let shell = process(pid: 48765, parent: 31389, group: 48700, startOffset: 0.2)
        XCTAssertEqual(ProcessKill.target(matched: shell, sessionPid: 31389), .success(48700))
    }
}
