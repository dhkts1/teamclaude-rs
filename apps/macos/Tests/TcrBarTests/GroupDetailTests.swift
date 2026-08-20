import XCTest

@testable import TcrBarCore

/// `Fleet.groupDetails` / `GroupDetail` — the Groups view's model layer.
/// Account names are obviously fake — this repository is public.
final class GroupDetailTests: XCTestCase {

    // MARK: - membership (bridge Unit 2)

    /// An account in two groups appears as a member of both.
    func testAccountInTwoGroupsAppearsInBothMemberLists() {
        let fleet = Fleet(accounts: [
            groupAccount("a@example.com", groups: ["codereview", "dev"])
        ])
        let byName = Dictionary(uniqueKeysWithValues: fleet.groupDetails.map { ($0.name, $0) })
        XCTAssertEqual(byName["codereview"]?.members.map(\.name), ["a@example.com"])
        XCTAssertEqual(byName["dev"]?.members.map(\.name), ["a@example.com"])
    }

    func testMembersIncludeDisabledAccounts() {
        let fleet = Fleet(accounts: [
            groupAccount("a@example.com", disabled: true, groups: ["dev"])
        ])
        XCTAssertEqual(fleet.groupDetails.first?.members.map(\.name), ["a@example.com"])
    }

    // MARK: - max 5h utilization

    /// The worst (highest) 5h fraction among members wins.
    func testMax5hPicksTheWorstMember() {
        let fleet = Fleet(accounts: [
            groupAccount("low@example.com", fiveHour: 0.10, groups: ["dev"]),
            groupAccount("high@example.com", fiveHour: 0.62, groups: ["dev"]),
        ])
        XCTAssertEqual(fleet.groupDetails.first?.max5hUtilization, 0.62)
    }

    func testMax5hIsNilWhenNoMemberHasEverReported() {
        let fleet = Fleet(accounts: [
            groupAccount("a@example.com", fiveHour: nil, groups: ["dev"])
        ])
        XCTAssertNil(fleet.groupDetails.first?.max5hUtilization)
    }

    // MARK: - soonest reset

    /// The earliest reset among held members wins, not the latest.
    func testSoonestResetPicksTheEarliest() {
        let fleet = Fleet(accounts: [
            groupAccount(
                "later@example.com",
                held: [HeldWindow(window: "5h", minutesUntilReset: 300, resetAtMs: 2)],
                groups: ["dev"]
            ),
            groupAccount(
                "sooner@example.com",
                held: [HeldWindow(window: "5h", minutesUntilReset: 134, resetAtMs: 1)],
                groups: ["dev"]
            ),
        ])
        XCTAssertEqual(fleet.groupDetails.first?.soonestReset?.minutesUntilReset, 134)
    }

    /// Omitted, not defaulted to some sentinel, when nothing in the group is
    /// held.
    func testSoonestResetIsNilWhenNoMemberIsHeld() {
        let fleet = Fleet(accounts: [
            groupAccount("a@example.com", groups: ["dev"])
        ])
        XCTAssertNil(fleet.groupDetails.first?.soonestReset)
    }

    // MARK: - stat line (bridge Unit 3): omit a missing half, never a placeholder

    func testStatLineJoinsBothHalvesWhenBothPresent() {
        let fleet = Fleet(accounts: [
            groupAccount(
                "a@example.com",
                fiveHour: 0.12,
                held: [HeldWindow(window: "5h", minutesUntilReset: 134, resetAtMs: 1)],
                groups: ["codereview"]
            )
        ])
        XCTAssertEqual(fleet.groupDetails.first?.statLine, "5h 12% max · resets in 2h 14m")
    }

    func testStatLineOmitsUtilizationHalfWhenNoMemberHasOne() {
        let fleet = Fleet(accounts: [
            groupAccount(
                "a@example.com",
                fiveHour: nil,
                held: [HeldWindow(window: "5h", minutesUntilReset: 60, resetAtMs: 1)],
                groups: ["dev"]
            )
        ])
        XCTAssertEqual(fleet.groupDetails.first?.statLine, "resets in 1h")
    }

    func testStatLineOmitsResetHalfWhenNothingIsHeld() {
        let fleet = Fleet(accounts: [
            groupAccount("a@example.com", fiveHour: 0.5, groups: ["dev"])
        ])
        XCTAssertEqual(fleet.groupDetails.first?.statLine, "5h 50% max")
    }

    /// Neither half has anything to say — the whole line is `nil`, not an
    /// empty string or a placeholder.
    func testStatLineIsNilWhenNeitherHalfHasAnything() {
        let fleet = Fleet(accounts: [
            groupAccount("a@example.com", fiveHour: nil, groups: ["dev"])
        ])
        XCTAssertNil(fleet.groupDetails.first?.statLine)
    }

    // MARK: - ordering (bridge Unit 4) — same as `groupBreakdown`'s existing rule

    func testGroupDetailsSortAlphabeticallyBeforeUngrouped() {
        let fleet = Fleet(accounts: [
            groupAccount("a@example.com", groups: ["zebra"]),
            groupAccount("b@example.com", groups: ["alpha"]),
            groupAccount("c@example.com", groups: nil),
        ])
        XCTAssertEqual(fleet.groupDetails.map(\.name), ["alpha", "zebra", "ungrouped"])
    }

    func testGroupDetailsAreEmptyWhenNoAccountAnywhereCarriesALabel() {
        let fleet = Fleet(accounts: [
            groupAccount("a@example.com", groups: nil),
            groupAccount("b@example.com", groups: []),
        ])
        XCTAssertEqual(fleet.groupDetails, [])
    }

    /// `groupBreakdown` — the pre-existing top-line summary — must still
    /// report the same free/total pairs now that it is derived from
    /// `groupDetails` instead of its own pass.
    func testGroupBreakdownStillMatchesGroupDetails() {
        let fleet = Fleet(accounts: [
            groupAccount("a@example.com", quotaState: .spent, groups: ["codereview"]),
            groupAccount("b@example.com", disabled: true, groups: ["codereview"]),
        ])
        XCTAssertEqual(fleet.groupBreakdown, [GroupTally(name: "codereview", free: 0, total: 2)])
        XCTAssertEqual(fleet.groupDetails.map(\.tally), fleet.groupBreakdown)
    }

    // MARK: - default expansion (bridge Unit 5)

    func testFreeIsZeroWhenEveryEnabledMemberIsSpent() {
        let fleet = Fleet(accounts: [
            groupAccount("a@example.com", quotaState: .spent, groups: ["codereview"])
        ])
        XCTAssertEqual(fleet.groupDetails.first?.free, 0)
    }

    func testFreeIsNonzeroWhenAMemberCanServe() {
        let fleet = Fleet(accounts: [
            groupAccount("a@example.com", quotaState: .ok, groups: ["codereview"])
        ])
        XCTAssertNotEqual(fleet.groupDetails.first?.free, 0)
    }

    /// A starved group (`free == 0`) starts expanded — the model property the
    /// view's disclosure row reads directly, per this file's house rule that
    /// a rendered/behavioural fact is a property of the model, not a
    /// view-private computation.
    func testStarvedGroupStartsExpanded() {
        let fleet = Fleet(accounts: [
            groupAccount("a@example.com", quotaState: .spent, groups: ["codereview"])
        ])
        XCTAssertEqual(fleet.groupDetails.first?.startsExpanded, true)
    }

    func testNonStarvedGroupStartsCollapsed() {
        let fleet = Fleet(accounts: [
            groupAccount("a@example.com", quotaState: .ok, groups: ["codereview"])
        ])
        XCTAssertEqual(fleet.groupDetails.first?.startsExpanded, false)
    }

    // MARK: - ungrouped ordering and absence (bridge Unit 4, continued)

    /// `ungrouped` is not just sorted alphabetically among real groups — it
    /// is forced to the end regardless of its own name, which would sort
    /// before every group in this fixture if it were treated as an ordinary
    /// key.
    func testUngroupedSortsLastEvenWhenItWouldSortFirstAlphabetically() {
        let fleet = Fleet(accounts: [
            groupAccount("a@example.com", groups: ["zzz-last-alphabetically"]),
            groupAccount("b@example.com", groups: nil),
        ])
        XCTAssertEqual(fleet.groupDetails.map(\.name), ["zzz-last-alphabetically", "ungrouped"])
    }

    /// No account anywhere carries a label: the whole list is absent, not an
    /// all-`ungrouped` list of one.
    func testGroupDetailsAbsentWhenNothingIsLabelled() {
        let fleet = Fleet(accounts: [
            groupAccount("a@example.com", groups: nil)
        ])
        XCTAssertEqual(fleet.groupDetails, [])
    }
}

/// Hand-built accounts with the group-detail-relevant fields exposed. Kept
/// separate from `FleetStatusTests`' own `account(...)` helper (private to
/// that file) rather than widening it — this file needs `fiveHour` and
/// `quotaState` control that helper does not expose.
private func groupAccount(
    _ name: String,
    quotaState: QuotaState = .ok,
    disabled: Bool = false,
    fiveHour: Double? = 0,
    held: [HeldWindow] = [],
    groups: [String]?
) -> Account {
    Account(
        name: name,
        priority: 1,
        status: "active",
        disabled: disabled,
        quota: 0,
        quotaState: quotaState,
        fiveHour: fiveHour,
        sevenDay: 0,
        sevenDayOi: 0,
        held: held,
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
        groups: groups
    )
}
