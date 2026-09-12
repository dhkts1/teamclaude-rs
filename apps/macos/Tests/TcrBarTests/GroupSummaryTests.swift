import XCTest

@testable import TcrBarCore

/// ``GroupSummary/summarize(_:)`` — every group name the fleet's accounts
/// carry, once each, with the facts the Settings window's Groups & Rotation
/// pane needs. The mockup review's S8 finding is the reason this derives from
/// the FULL account list rather than whatever the panel happens to have drawn
/// (a collapsed group card, or one filtered view): two groups there existed
/// only as bare names with none of their settings shown.
///
/// Account names are obviously fake — this repository is public.
final class GroupSummaryTests: XCTestCase {

    func testEveryGroupNameIsCounted() {
        let accounts = [
            summaryAccount("a@example.com", groups: ["dev"]),
            summaryAccount("b@example.com", groups: ["dev", "gil"]),
            summaryAccount("c@example.com", groups: ["gil"]),
        ]
        let groups = GroupSummary.summarize(accounts)
        XCTAssertEqual(groups.map(\.name), ["dev", "gil"])
        XCTAssertEqual(groups[0].memberCount, 2)
        XCTAssertEqual(groups[1].memberCount, 2)
    }

    /// A group with no member carrying `parkedGroups` for it is not parked —
    /// the negative case, since every other fixture here opts in.
    func testAGroupWithNoParkedMemberIsNotParked() {
        let accounts = [summaryAccount("a@example.com", groups: ["dev"])]
        XCTAssertEqual(GroupSummary.summarize(accounts).first?.isParked, false)
    }

    func testAGroupIsParkedWhenAMemberCarriesIt() {
        let accounts = [
            summaryAccount("a@example.com", groups: ["dev"], parkedGroups: ["dev"])
        ]
        XCTAssertEqual(GroupSummary.summarize(accounts).first?.isParked, true)
    }

    func testAGroupIsReservedWhenAMemberCarriesIt() {
        let accounts = [
            summaryAccount("a@example.com", groups: ["dev"], reservedGroups: ["dev"])
        ]
        XCTAssertEqual(GroupSummary.summarize(accounts).first?.isReserved, true)
    }

    /// The colour comes from the first member that carries one for this
    /// group; a group with no coloured member reports `nil`, the same
    /// neutral-fallback shape ``GroupChip`` already uses.
    func testTheGroupColorIsReadFromAMemberThatCarriesOne() {
        let accounts = [
            summaryAccount("a@example.com", groups: ["dev"]),
            summaryAccount("b@example.com", groups: ["dev"], groupColors: ["dev": "#0a84ff"]),
        ]
        XCTAssertEqual(GroupSummary.summarize(accounts).first?.colorHex, "#0a84ff")
    }

    func testAGroupWithNoColoredMemberReportsNilColor() {
        let accounts = [summaryAccount("a@example.com", groups: ["dev"])]
        XCTAssertNil(GroupSummary.summarize(accounts).first?.colorHex)
    }

    /// Sorted by name, so the pane's row order does not depend on account
    /// order or on `Set` iteration order.
    func testGroupsAreSortedByName() {
        let accounts = [
            summaryAccount("a@example.com", groups: ["zeta"]),
            summaryAccount("b@example.com", groups: ["alpha"]),
        ]
        XCTAssertEqual(GroupSummary.summarize(accounts).map(\.name), ["alpha", "zeta"])
    }

    func testNoGroupsProducesAnEmptyList() {
        XCTAssertEqual(GroupSummary.summarize([summaryAccount("a@example.com", groups: nil)]), [])
    }
}

/// Hand-built accounts with the fields this file's assertions touch —
/// mirrors `GroupRoutingTests`'s own `routingAccount` fixture.
private func summaryAccount(
    _ name: String,
    groups: [String]?,
    reservedGroups: [String]? = nil,
    parkedGroups: [String]? = nil,
    groupColors: [String: String]? = nil
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
        reservedGroups: reservedGroups,
        parkedGroups: parkedGroups,
        groupColors: groupColors
    )
}
