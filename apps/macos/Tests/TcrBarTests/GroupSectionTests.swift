import XCTest

@testable import TcrBarCore

/// `Fleet.groupSections` / `GroupSection` — the accounts list's group-SET
/// sectioning, which replaced the separate `Accounts | Groups` toggle
/// (bridge: `docs/plans/group-sections-bridge.md`). Account names are
/// obviously fake — this repository is public.
final class GroupSectionTests: XCTestCase {

    // MARK: - Unit 1: an account in two groups appears exactly once

    func testAccountInTwoGroupsAppearsExactlyOnceUnderTheCombinedHeader() {
        let fleet = Fleet(accounts: [
            sectionAccount("both@example.com", groups: ["dev", "codereview"]),
            sectionAccount("codereviewOnly@example.com", groups: ["codereview"]),
        ])
        // Sorted alphabetically before joining, regardless of input order.
        let combined = fleet.groupSections.first { $0.header == "codereview + dev" }
        XCTAssertEqual(combined?.members.map(\.name), ["both@example.com"])
        // Not also counted under the single-group "codereview" section.
        let codereviewOnly = fleet.groupSections.first { $0.header == "codereview" }
        XCTAssertEqual(codereviewOnly?.members.map(\.name), ["codereviewOnly@example.com"])
        // Exactly one section total carries the two-group account.
        let sectionsContainingBoth = fleet.groupSections.filter {
            $0.members.contains { $0.name == "both@example.com" }
        }
        XCTAssertEqual(sectionsContainingBoth.count, 1)
    }

    // MARK: - Unit 2: section order — alphabetical by header, ungrouped last

    func testSectionsSortAlphabeticallyByHeaderWithUngroupedLast() {
        let fleet = Fleet(accounts: [
            sectionAccount("a@example.com", groups: ["zebra"]),
            sectionAccount("b@example.com", groups: ["alpha"]),
            sectionAccount("c@example.com", groups: nil),
        ])
        XCTAssertEqual(fleet.groupSections.map(\.header), ["alpha", "zebra", "ungrouped"])
    }

    func testUngroupedSortsLastEvenWhenItWouldSortFirstAlphabetically() {
        let fleet = Fleet(accounts: [
            sectionAccount("a@example.com", groups: ["zzz-last-alphabetically"]),
            sectionAccount("b@example.com", groups: nil),
        ])
        XCTAssertEqual(fleet.groupSections.map(\.header), ["zzz-last-alphabetically", "ungrouped"])
    }

    func testCombinedHeaderJoinsMemberGroupsSortedAlphabeticallyRegardlessOfWireOrder() {
        let fleet = Fleet(accounts: [
            sectionAccount("a@example.com", groups: ["dev", "codereview"])
        ])
        XCTAssertEqual(fleet.groupSections.map(\.header), ["codereview + dev"])
    }

    // MARK: - Unit 3: no labels anywhere → zero sections, flat list unchanged

    func testNoAccountAnywhereCarryingALabelProducesNoSections() {
        let fleet = Fleet(accounts: [
            sectionAccount("a@example.com", groups: nil),
            sectionAccount("b@example.com", groups: []),
        ])
        XCTAssertEqual(fleet.groupSections, [])
        XCTAssertFalse(fleet.groupSectionsFragmented)
        XCTAssertNil(fleet.groupSectionsFragmentedNotice)
    }

    // MARK: - Unit 4: fragmentation guard — over the threshold falls back to flat

    func testMoreThanThresholdDistinctGroupSetsFallsBackToFlat() {
        // One distinct group-set per account, well past the threshold of 8.
        let accounts = (0..<(Fleet.groupSetFragmentationThreshold + 1)).map { index in
            sectionAccount("acct\(index)@example.com", groups: ["group\(index)"])
        }
        let fleet = Fleet(accounts: accounts)
        XCTAssertTrue(fleet.groupSectionsFragmented)
        XCTAssertEqual(fleet.groupSections, [])
        XCTAssertEqual(fleet.groupSectionsFragmentedNotice, "too many group combinations to section")
    }

    func testExactlyAtTheThresholdStillSections() {
        let accounts = (0..<Fleet.groupSetFragmentationThreshold).map { index in
            sectionAccount("acct\(index)@example.com", groups: ["group\(index)"])
        }
        let fleet = Fleet(accounts: accounts)
        XCTAssertFalse(fleet.groupSectionsFragmented)
        XCTAssertEqual(fleet.groupSections.count, Fleet.groupSetFragmentationThreshold)
        XCTAssertNil(fleet.groupSectionsFragmentedNotice)
    }

    // MARK: - Unit 5: free/total — disabled counts in total, not free

    func testFreeExcludesDisabledButTotalIncludesIt() {
        let fleet = Fleet(accounts: [
            sectionAccount("ok@example.com", quotaState: .ok, groups: ["codereview"]),
            sectionAccount("disabled@example.com", disabled: true, groups: ["codereview"]),
        ])
        let section = fleet.groupSections.first { $0.header == "codereview" }
        XCTAssertEqual(section?.total, 2)
        XCTAssertEqual(section?.free, 1)
    }

    func testFreeCountsAnEnabledNearAccountAsFreeLikeTheTallyDoes() {
        let fleet = Fleet(accounts: [
            sectionAccount("near@example.com", quotaState: .near, groups: ["dev"])
        ])
        let section = fleet.groupSections.first { $0.header == "dev" }
        XCTAssertEqual(section?.free, 1)
    }

    // MARK: - Unit 6: reservedGroups — decodes when present, absent-tolerant, marks the right section

    func testReservedGroupDecodesAndMarksItsOwnSection() {
        let fleet = Fleet(accounts: [
            sectionAccount("a@example.com", groups: ["codereview"], reservedGroups: ["codereview"]),
            sectionAccount("b@example.com", groups: ["dev"]),
        ])
        let reserved = fleet.groupSections.first { $0.header == "codereview" }
        XCTAssertEqual(reserved?.isReserved, true)
        XCTAssertEqual(reserved?.reservedGroups, ["codereview"])
        let notReserved = fleet.groupSections.first { $0.header == "dev" }
        XCTAssertEqual(notReserved?.isReserved, false)
        XCTAssertEqual(notReserved?.reservedGroups, [])
    }

    /// A combined section is reserved only through the groups actually marked
    /// — a section with one reserved and one unreserved member group reports
    /// just the reserved one, not both.
    func testCombinedSectionReportsOnlyItsReservedMemberGroup() {
        let fleet = Fleet(accounts: [
            sectionAccount(
                "a@example.com", groups: ["codereview", "dev"], reservedGroups: ["codereview"])
        ])
        let section = fleet.groupSections.first { $0.header == "codereview + dev" }
        XCTAssertEqual(section?.reservedGroups, ["codereview"])
        XCTAssertEqual(section?.isReserved, true)
    }

    /// `nil` `reservedGroups` — the shape a server predating the field sends
    /// (`decodeIfPresent` never fails the row) — degrades to "nothing
    /// reserved", not a decode failure.
    func testNilReservedGroupsDegradesToNothingReserved() {
        let fleet = Fleet(accounts: [
            sectionAccount("a@example.com", groups: ["codereview"], reservedGroups: nil)
        ])
        let section = fleet.groupSections.first { $0.header == "codereview" }
        XCTAssertEqual(section?.isReserved, false)
    }

    /// A row that omits `reservedGroups` on the wire entirely — not merely
    /// `null` — must still decode. Synthesized `Decodable` calls
    /// `decodeIfPresent` for an `Optional` property, so a missing key is
    /// exactly as safe as `groups`'s own forward-compat contract.
    func testAccountRowMissingReservedGroupsKeyEntirelyStillDecodes() throws {
        let json = #"""
            [
              {
                "name": "a@example.com",
                "priority": 0,
                "status": "active",
                "disabled": false,
                "quota": null,
                "quotaState": "ok",
                "fiveHour": null,
                "sevenDay": null,
                "sevenDayOi": null,
                "held": [],
                "requests": null,
                "inputTokens": null,
                "outputTokens": null,
                "cacheReadTokens": null,
                "cacheHitRatio": null,
                "probeStatus": "never",
                "probeError": null,
                "lastStreamError": null,
                "streamErrorCount": null,
                "source": "live",
                "serverSha": null,
                "serverDirty": null,
                "groups": ["codereview"]
              }
            ]
            """#
        let fleet = try Fleet.decode(Data(json.utf8))
        XCTAssertEqual(fleet.unreadableCount, 0)
        XCTAssertNil(fleet.accounts.first?.reservedGroups)
        XCTAssertEqual(fleet.groupSections.first?.isReserved, false)
    }
}

/// Hand-built accounts with the group-set-relevant fields exposed. Mirrors
/// `GroupDetailTests`' own `groupAccount(...)` helper, kept separate because
/// this file additionally needs `reservedGroups` control that helper does
/// not expose.
private func sectionAccount(
    _ name: String,
    quotaState: QuotaState = .ok,
    disabled: Bool = false,
    groups: [String]?,
    reservedGroups: [String]? = nil
) -> Account {
    Account(
        name: name,
        priority: 1,
        status: "active",
        disabled: disabled,
        quota: 0,
        quotaState: quotaState,
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
        reservedGroups: reservedGroups
    )
}
