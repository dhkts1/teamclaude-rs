import XCTest

@testable import TcrBarCore

/// Regression coverage for two render fixture pairs that used to draw the
/// same picture under two different scene names, caught by checksumming
/// every PNG `--render-states` writes and finding two DIFFERENT names with
/// the SAME bytes, both appearances.
///
/// The render harness itself lives in the `TcrBar` executable target and is
/// not importable here (the test target links `TcrBarCore` alone,
/// `Package.swift`'s own comment on why). These tests instead pin the two
/// pieces of `TcrBarCore` logic each duplicate turned on, the awake window
/// and the wholly-parked group's collapse threshold, so a change that
/// silently reintroduces either duplicate fails here first, before a PNG
/// diff would be needed to notice it.
///
/// Account and peer names below are obviously fake: this repository is
/// public.
final class RenderFixtureShapeTests: XCTestCase {

    // MARK: - The asleep / unreachable pair

    /// The peer render fixtures distinguish "awake but unreachable" from
    /// "asleep" entirely through `PeerFormat.sinceLastSeen`'s freshness
    /// window: a `lastSeenMs` a couple of seconds old reads as awake, one
    /// six minutes old does not. Before the fixture fix both scenes pinned a
    /// six-minute-old `lastSeenMs`, so this same call returned `awake: false`
    /// for both and there was nothing left in the data to draw differently.
    func testRecentLastSeenReadsAwakeAndStaleDoesNot() {
        let now = Date(timeIntervalSince1970: 1_786_000_000)

        let recent = PeerFormat.sinceLastSeen(
            Int64((now.timeIntervalSince1970 - 2) * 1000), now: now)
        let stale = PeerFormat.sinceLastSeen(
            Int64((now.timeIntervalSince1970 - 360) * 1000), now: now)

        XCTAssertTrue(recent.awake, "2s old must read as awake, the unreachable fixture's case")
        XCTAssertFalse(stale.awake, "360s old must read as asleep, distinct from the case above")
    }

    // MARK: - The parked group collapse / expand pair

    /// A wholly-parked named group only draws differently collapsed versus
    /// expanded when it holds MORE rows than the collapsed view shows before
    /// its "Show N more accounts" button: three, `FleetView.parkedVisibleRows`
    /// (not importable here; TcrBar-target constant, pinned as a literal with
    /// this comment as the cross-reference). At or under three, every row is
    /// visible either way and the collapsed and expanded renders are the same
    /// picture, which is exactly the fixture bug this test guards: the old
    /// fixture parked two `henry-team` accounts, so there was nothing left to
    /// reveal on expansion.
    func testWhollyParkedGroupAboveVisibleRowsHasHiddenAccounts() {
        let parkedVisibleRows = 3
        let fleet = Fleet(accounts: [
            sectionAccount("alice@example.com", groups: ["henry-team"], parkedGroups: ["henry-team"]),
            sectionAccount("bob@example.com", groups: ["henry-team"], parkedGroups: ["henry-team"]),
            sectionAccount("erin@example.com", groups: ["henry-team"], parkedGroups: ["henry-team"]),
            sectionAccount("frank@example.com", groups: ["henry-team"], parkedGroups: ["henry-team"]),
        ])
        let section = try! XCTUnwrap(
            fleet.sectionsInDisplayOrder().first { $0.group == .named("henry-team") })

        XCTAssertTrue(section.isWhollyParked)
        XCTAssertGreaterThan(
            section.rows.count, parkedVisibleRows,
            "four parked rows must exceed the three-row collapse threshold, or collapsed and "
                + "expanded draw the same set of rows again")
    }

    /// The shape the OLD fixture had, kept as its own test rather than
    /// deleted: two members of a wholly-parked group is a state this panel
    /// can still be in, and it is the one that must NOT be mistaken for a
    /// render-review fixture again, since collapsing it changes nothing on
    /// screen.
    func testTwoMemberParkedGroupHasNothingLeftToReveal() {
        let parkedVisibleRows = 3
        let fleet = Fleet(accounts: [
            sectionAccount("alice@example.com", groups: ["henry-team"], parkedGroups: ["henry-team"]),
            sectionAccount("bob@example.com", groups: ["henry-team"], parkedGroups: ["henry-team"]),
        ])
        let section = try! XCTUnwrap(
            fleet.sectionsInDisplayOrder().first { $0.group == .named("henry-team") })

        XCTAssertTrue(section.isWhollyParked)
        XCTAssertLessThanOrEqual(
            section.rows.count, parkedVisibleRows,
            "two rows sit at or under the collapse threshold: every row already renders "
                + "collapsed, so this shape must never be the fixture behind a collapse/expand pair")
    }
}

private func sectionAccount(
    _ name: String,
    groups: [String]?,
    parkedGroups: [String]? = nil
) -> Account {
    Account(
        name: name,
        priority: 1,
        status: "active",
        disabled: false,
        quota: 0,
        quotaState: .ok,
        fiveHour: 0,
        sevenDay: 0,
        sevenDayOi: 0,
        held: [],
        requests: 0,
        inputTokens: 0,
        outputTokens: 0,
        cacheReadTokens: 0,
        cacheHitRatio: nil,
        probeStatus: .ok,
        probeError: nil,
        lastStreamError: nil,
        streamErrorCount: 0,
        source: .live,
        serverSha: "abc1234",
        serverDirty: false,
        groups: groups,
        reservedGroups: nil,
        parkedGroups: parkedGroups,
        groupColors: nil,
        usage: nil
    )
}
