import XCTest

@testable import TcrBarCore

/// ``SessionSections/byProject(_:)`` — the Sessions tab's project grouping.
/// Fixtures use obviously-fake session ids, account names and project paths
/// only (this repo is public — see CLAUDE.md).
final class SessionSectionsTests: XCTestCase {
    private func joined(
        id: String, account: String? = nil, cwd: String?, costUsd: Double? = nil,
        errors: Int = 0
    ) -> JoinedSession {
        JoinedSession(
            session: Session(
                sessionId: id, account: account, firstSeenMs: 0, lastSeenMs: 0,
                tools: SessionTools(errors: errors), costUsd: costUsd),
            file: SessionFile(sessionId: id, cwd: cwd))
    }

    /// The measured shape this unit exists for: grouping by project splits a
    /// fleet an account-keyed grouping would have piled into one bucket.
    func testSessionsGroupByProjectNotAccount() {
        let sessions = [
            joined(id: "s1", account: "acct-a", cwd: "/Users/alice/token", costUsd: 5),
            joined(id: "s2", account: "acct-a", cwd: "/Users/alice/token", costUsd: 3),
            joined(id: "s3", account: "acct-a", cwd: "/Users/alice/teamclaude-rs", costUsd: 1),
        ]
        let sections = SessionSections.byProject(sessions)
        XCTAssertEqual(sections.map(\.key), [.named("token"), .named("teamclaude-rs")])
        XCTAssertEqual(sections[0].rows.map(\.id), ["s1", "s2"])
    }

    /// A project with fewer than two sessions draws no header — its session
    /// is still IN the list, just flagged as headerless, so the renderer can
    /// draw it as a plain tagged row instead of a one-row card.
    func testAProjectWithFewerThanTwoSessionsHasNoHeader() {
        let sessions = [
            joined(id: "s1", cwd: "/Users/alice/token"),
            joined(id: "s2", cwd: "/Users/alice/token"),
            joined(id: "s3", cwd: "/Users/alice/widgets"),
        ]
        let sections = SessionSections.byProject(sessions)
        let token = try! XCTUnwrap(sections.first { $0.key == .named("token") })
        let widgets = try! XCTUnwrap(sections.first { $0.key == .named("widgets") })
        XCTAssertTrue(token.hasHeader)
        XCTAssertFalse(widgets.hasHeader)
    }

    /// A session with no file, or a file with no `cwd`, has no project — those
    /// collapse into ONE `.unassigned` bucket rather than each becoming its
    /// own headerless singleton, matching ``FleetGroupKey/ungrouped``'s own
    /// collapsing rule for accounts.
    func testSessionsWithNoProjectCollapseIntoOneUnassignedBucket() {
        let sessions = [
            joined(id: "s1", cwd: nil),
            joined(id: "s2", cwd: ""),
        ]
        let sections = SessionSections.byProject(sessions)
        XCTAssertEqual(sections.count, 1)
        XCTAssertEqual(sections[0].key, .unassigned)
        XCTAssertEqual(sections[0].rows.count, 2)
    }

    /// Sections order by spend, highest first — the bridge's own rule — and a
    /// section with nothing priced sorts LAST rather than claiming a $0 spend
    /// it never measured.
    func testSectionsOrderBySpendHighestFirstAndUnpricedLast() {
        let sessions = [
            joined(id: "s1", cwd: "/Users/alice/low", costUsd: 1),
            joined(id: "s2", cwd: "/Users/alice/high", costUsd: 100),
            joined(id: "s3", cwd: "/Users/alice/unpriced", costUsd: nil),
        ]
        let sections = SessionSections.byProject(sessions)
        XCTAssertEqual(
            sections.map(\.key), [.named("high"), .named("low"), .named("unpriced")])
    }

    func testHeaderSummaryNamesCountSpendAndErrors() {
        let sessions = [
            joined(id: "s1", cwd: "/Users/alice/token", costUsd: 500, errors: 100),
            joined(id: "s2", cwd: "/Users/alice/token", costUsd: 480, errors: 66),
        ]
        let sections = SessionSections.byProject(sessions)
        XCTAssertEqual(sections[0].headerSummary, "2 · $980 · 166 err")
    }

    /// Zero errors drops the clause entirely rather than printing "0 err".
    func testHeaderSummaryDropsTheErrorClauseAtZero() {
        let sessions = [
            joined(id: "s1", cwd: "/Users/alice/token", costUsd: 10),
            joined(id: "s2", cwd: "/Users/alice/token", costUsd: 10),
        ]
        let sections = SessionSections.byProject(sessions)
        XCTAssertEqual(sections[0].headerSummary, "2 · $20.0")
    }
}
