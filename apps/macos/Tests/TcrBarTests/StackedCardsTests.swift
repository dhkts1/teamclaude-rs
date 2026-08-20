import XCTest

@testable import TcrBarCore

/// `GroupSection.summaryLine`/`accountCountLabel`/`max5hUtilization` — the
/// stacked deck card's collapsed summary line — and
/// `Account.groupMenuActions` — the row-level right-click menu that changes
/// group membership (bridge: `docs/plans/stacked-cards-bridge.md`). Account
/// names are obviously fake — this repository is public.
final class StackedCardsTests: XCTestCase {

    // MARK: - collapsed card summary: count, free/total, worst-window

    /// Both halves present: count and worst-window join with " · ", the
    /// same idiom `GroupDetail.statLine` already uses.
    func testSummaryLineJoinsCountAndWorstWindowWhenBothPresent() {
        let fleet = Fleet(accounts: [
            deckAccount("low@example.com", fiveHour: 0.10, groups: ["dev"]),
            deckAccount("high@example.com", fiveHour: 0.33, groups: ["dev"]),
        ])
        let section = fleet.groupSections.first { $0.header == "dev" }
        XCTAssertEqual(section?.accountCountLabel, "2 accounts")
        XCTAssertEqual(section?.summaryLine, "2 accounts · 5h 33% max")
    }

    /// The worst-window half is OMITTED, not printed as a placeholder, when
    /// no member has ever reported a 5h fraction — the count half never
    /// disappears, since a section always has at least one member.
    func testSummaryLineOmitsWorstWindowWhenNoMemberHasEverReported() {
        let fleet = Fleet(accounts: [
            deckAccount("a@example.com", fiveHour: nil, groups: ["dev"])
        ])
        let section = fleet.groupSections.first { $0.header == "dev" }
        XCTAssertNil(section?.max5hUtilization)
        XCTAssertEqual(section?.summaryLine, "1 account")
    }

    /// A single-account set still renders sensibly: singular noun, and the
    /// worst-window half present when that one member has a reading.
    func testSingleAccountSetRendersSensibly() {
        let fleet = Fleet(accounts: [
            deckAccount("solo@example.com", fiveHour: 0.10, groups: ["codereview", "dev"])
        ])
        let section = fleet.groupSections.first { $0.header == "codereview + dev" }
        XCTAssertEqual(section?.total, 1)
        XCTAssertEqual(section?.accountCountLabel, "1 account")
        XCTAssertEqual(section?.summaryLine, "1 account · 5h 10% max")
    }

    /// The worst (highest) 5h fraction among members wins, same rule
    /// `GroupDetail.max5hUtilization` already applies.
    func testMax5hUtilizationPicksTheWorstMember() {
        let fleet = Fleet(accounts: [
            deckAccount("low@example.com", fiveHour: 0.05, groups: ["dev"]),
            deckAccount("high@example.com", fiveHour: 0.97, groups: ["dev"]),
        ])
        let section = fleet.groupSections.first { $0.header == "dev" }
        XCTAssertEqual(section?.max5hUtilization, 0.97)
    }

    // MARK: - right-click row menu (bridge: the missing affordance)

    /// An account in two groups lists a "Remove from <group>" for each,
    /// plus "Remove from all groups", plus "Add to group…" last.
    func testAccountInTwoGroupsListsBothRemovalsPlusRemoveAllPlusAdd() {
        let account = deckAccount("a@example.com", groups: ["dev", "codereview"])
        XCTAssertEqual(
            account.groupMenuActions,
            [
                .remove(group: "codereview"),
                .remove(group: "dev"),
                .removeAll,
                .addToGroup,
            ]
        )
    }

    /// A single membership gets its own removal action but NOT
    /// "Remove from all groups" — a second control doing the exact same
    /// thing as the first is noise, not a convenience.
    func testAccountInOneGroupOffersNoRemoveAll() {
        let account = deckAccount("a@example.com", groups: ["dev"])
        XCTAssertEqual(account.groupMenuActions, [.remove(group: "dev"), .addToGroup])
    }

    /// An account in no group offers only "Add to group…" — the missing
    /// affordance the bridge exists to add, with nothing to remove.
    func testAccountInNoGroupOffersOnlyAddToGroup() {
        let account = deckAccount("a@example.com", groups: nil)
        XCTAssertEqual(account.groupMenuActions, [.addToGroup])
    }

    /// Same for an explicitly empty (not nil) groups array — the wire's
    /// other spelling of "no membership".
    func testAccountWithEmptyGroupsArrayOffersOnlyAddToGroup() {
        let account = deckAccount("a@example.com", groups: [])
        XCTAssertEqual(account.groupMenuActions, [.addToGroup])
    }
}

/// Hand-built accounts with the fields this file's assertions touch.
/// Mirrors `GroupSectionTests`' own `sectionAccount(...)` helper, kept
/// separate because this file additionally needs `fiveHour` control that
/// helper does not expose.
private func deckAccount(
    _ name: String,
    fiveHour: Double? = 0,
    groups: [String]?
) -> Account {
    Account(
        name: name,
        priority: 1,
        status: "active",
        disabled: false,
        quota: 0,
        quotaState: .ok,
        fiveHour: fiveHour,
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
        groups: groups
    )
}
