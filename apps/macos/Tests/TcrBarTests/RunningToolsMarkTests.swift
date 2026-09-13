import XCTest

@testable import TcrBarCore

/// The two pure facts the menu-bar mark's running-tools segment and amber rule
/// (F5 remainder, `data/plans/menubar-mark-bridge.md`) rest on:
/// ``PollState/runningToolsCount(showRunningTools:)`` and
/// ``PollState/countIsNearCapacity``. The rendered title and glyph — the
/// AppKit half — are `TcrBar`, not `TcrBarCore`, so they are reviewed through
/// `--render-mark` fixtures and `--shell-probe` instead (`RenderMark.swift`'s
/// own doc-comment on why the test target cannot link that executable
/// target). What lives here is the state logic those fixtures both draw from,
/// checked directly rather than only through a picture of it.
@MainActor
final class RunningToolsMarkTests: XCTestCase {

    private func account(
        name: String, quotaState: String, probeStatus: String, disabled: Bool = false
    ) -> Account {
        Account(
            name: name, priority: 1, status: "active", disabled: disabled,
            quota: 0.5, quotaState: QuotaState(token: quotaState),
            fiveHour: 0.5, sevenDay: 0.5, sevenDayOi: 0.0,
            held: [], requests: 0, inputTokens: 0, outputTokens: 0, cacheReadTokens: 0,
            cacheHitRatio: nil, probeStatus: ProbeState(token: probeStatus), probeError: nil,
            lastStreamError: nil, streamErrorCount: 0, source: .live, serverSha: nil,
            serverDirty: nil)
    }

    private func session(running: Bool) -> Session {
        Session(
            sessionId: "s-\(UUID().uuidString)", firstSeenMs: 0, lastSeenMs: 0,
            tools: running
                ? SessionTools(calls: 1, running: [ToolCall(tool: "Bash")]) : SessionTools())
    }

    // MARK: - runningToolsCount

    /// The mockup's own gate, verbatim: "appears only when the preference is
    /// on AND the wire carries `sessions`" — both conditions independently
    /// required, not either.
    func testRunningToolsCountIsNilWhenThePreferenceIsOff() {
        let fleet = Fleet(
            accounts: [account(name: "a@example.com", quotaState: "ok", probeStatus: "ok")],
            sessions: [session(running: true)], sessionsSupported: true)
        XCTAssertNil(PollState.loaded(fleet).runningToolsCount(showRunningTools: false))
    }

    /// `sessionsSupported == false` is not "zero running" — it is "this read
    /// carries no opinion at all" (``Fleet/sessions``'s own doc-comment: every
    /// live `decode(_:)` today leaves it `false`). A preference switched on
    /// long before the wire exists must never draw a false `0`.
    func testRunningToolsCountIsNilWhenTheWireDoesNotCarrySessionsEvenWithSessionsPresent() {
        let fleet = Fleet(
            accounts: [account(name: "a@example.com", quotaState: "ok", probeStatus: "ok")],
            sessions: [session(running: true)], sessionsSupported: false)
        XCTAssertNil(PollState.loaded(fleet).runningToolsCount(showRunningTools: true))
    }

    func testRunningToolsCountIsNilForEveryNonLoadedState() {
        XCTAssertNil(PollState.pending.runningToolsCount(showRunningTools: true))
        XCTAssertNil(
            PollState.commandFailed(exitCode: 1, message: "x").runningToolsCount(
                showRunningTools: true))
        XCTAssertNil(
            PollState.toolMissing(searched: []).runningToolsCount(showRunningTools: true))
        XCTAssertNil(PollState.undecodable(message: "x").runningToolsCount(showRunningTools: true))
    }

    /// The segment's actual number: every running call across every session,
    /// pooled — the same count ``Fleet/toolsRunning`` already gives the Tools
    /// tab, not a second one authored here.
    func testRunningToolsCountPoolsAcrossSessionsWhenShown() {
        let fleet = Fleet(
            accounts: [account(name: "a@example.com", quotaState: "ok", probeStatus: "ok")],
            sessions: [session(running: true), session(running: true), session(running: false)],
            sessionsSupported: true)
        XCTAssertEqual(PollState.loaded(fleet).runningToolsCount(showRunningTools: true), 2)
    }

    /// Zero running is a real, drawable fact once the wire exists — distinct
    /// from `nil`, which means "do not draw the segment at all".
    func testRunningToolsCountCanBeZeroWithoutBeingNil() {
        let fleet = Fleet(
            accounts: [account(name: "a@example.com", quotaState: "ok", probeStatus: "ok")],
            sessions: [], sessionsSupported: true)
        XCTAssertEqual(PollState.loaded(fleet).runningToolsCount(showRunningTools: true), 0)
    }

    // MARK: - countIsNearCapacity (the amber rule)

    /// The mockup's rule, verbatim: "amber = none ready and some near". A
    /// fleet with a ready account must never draw amber even if a different
    /// account is near — `ok` outranks `near` on the glyph, and the count
    /// must agree with the glyph it sits beside.
    func testNotAmberWhenAnyAccountIsReadyEvenIfAnotherIsNear() {
        let fleet = Fleet(accounts: [
            account(name: "ready@example.com", quotaState: "ok", probeStatus: "ok"),
            account(name: "near@example.com", quotaState: "near", probeStatus: "ok"),
        ])
        XCTAssertFalse(PollState.loaded(fleet).countIsNearCapacity)
    }

    func testAmberWhenNoneReadyAndAtLeastOneNear() {
        let fleet = Fleet(accounts: [
            account(name: "spent@example.com", quotaState: "spent", probeStatus: "ok"),
            account(name: "near@example.com", quotaState: "near", probeStatus: "ok"),
        ])
        XCTAssertTrue(PollState.loaded(fleet).countIsNearCapacity)
    }

    /// None ready, none near (all spent, or all unmeasured) is `.spent` or
    /// `.unknown` on the glyph — a DIFFERENT state from `.near`, so the count
    /// must not also claim amber for it.
    func testNotAmberWhenNoneReadyAndNoneNear() {
        let fleet = Fleet(accounts: [
            account(name: "spent@example.com", quotaState: "spent", probeStatus: "ok")
        ])
        XCTAssertFalse(PollState.loaded(fleet).countIsNearCapacity)
    }

    func testNotAmberForEveryNonLoadedState() {
        XCTAssertFalse(PollState.pending.countIsNearCapacity)
        XCTAssertFalse(PollState.commandFailed(exitCode: 1, message: "x").countIsNearCapacity)
        XCTAssertFalse(PollState.toolMissing(searched: []).countIsNearCapacity)
        XCTAssertFalse(PollState.undecodable(message: "x").countIsNearCapacity)
    }
}
