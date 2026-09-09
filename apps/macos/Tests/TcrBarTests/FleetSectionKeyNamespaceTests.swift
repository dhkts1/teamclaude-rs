import XCTest

@testable import TcrBarCore

/// The key namespace `FleetView` keys its measured-height dictionary on.
///
/// `FleetView.rowHeights` is one `[String: CGFloat]` shared by three kinds of
/// list child: account rows under ``FleetSectionRow/id``, group headings under
/// `"h:" + FleetSection.id`, and band headings under `"b:" + FleetBand.rawValue`.
/// Two children sharing a key is not a cosmetic bug — the second measurement
/// overwrites the first, `visibleRowsHeight(for:)` then sums fewer heights than
/// there are children, and the scroll viewport comes out short of its own
/// content and clips the last card mid-line. That is the exact failure the
/// `controlHairline` term in ``PanelHeight`` already exists to undo once.
///
/// The view cannot be tested here — the test target links `TcrBarCore` only
/// (`Package.swift:39-43`) — so what is asserted is the half of the invariant
/// that lives in the library: the shape of the ids the view builds those keys
/// from. If a row id could begin `b:` or `h:`, the view's prefixes would stop
/// being safe and this file goes red before the panel does.
///
/// Account names are obviously fake — this repository is public.
final class FleetSectionKeyNamespaceTests: XCTestCase {

    /// The prefixes `FleetView` reserves for its heading keys. Named here so
    /// the test states what it is protecting rather than hiding it in a string.
    private let headingPrefixes = ["b:", "h:"]

    /// Every row id opens with a ``FleetGroupKey/token`` — `g:` or `u:` — so no
    /// row can ever land on a heading key, whatever the operator names a group.
    func testNoRowIdentityCanCollideWithAHeadingKey() {
        let fleet = Fleet(accounts: [
            sectionAccount("a@example.com", groups: ["b", "h"]),
            sectionAccount("b@example.com", groups: ["b:0", "h:0\u{1F}x"]),
            sectionAccount("c@example.com", groups: nil),
        ])

        let ids = fleet.sectionsInDisplayOrder().flatMap { $0.rows }.map(\.id)
        XCTAssertFalse(ids.isEmpty, "positive control: the fixture must produce rows")
        for id in ids {
            XCTAssertTrue(
                id.hasPrefix("g:") || id.hasPrefix("u:"),
                "row id \(id) left the group-token namespace the heading keys are safe against")
            for prefix in headingPrefixes {
                XCTAssertFalse(id.hasPrefix(prefix), "row id \(id) collides with heading prefix \(prefix)")
            }
        }
    }

    /// A section id opens with its band's raw value, so `"h:" + id` is likewise
    /// out of reach of every row key and of the `b:` band keys.
    func testSectionIdentitiesStayOutOfTheRowAndBandNamespaces() {
        let fleet = Fleet(accounts: [
            sectionAccount("live@example.com", groups: ["dev"]),
            sectionAccount("spent@example.com", groups: ["dev"], quotaState: .spent),
            sectionAccount("parked@example.com", groups: ["dev"], disabled: true),
        ])

        let sectionIds = fleet.sectionsInDisplayOrder().map(\.id)
        XCTAssertEqual(sectionIds.count, 3, "positive control: dev is split across all three bands")
        for id in sectionIds {
            XCTAssertTrue(
                id.first.map { $0.isNumber } == true,
                "section id \(id) no longer starts with its band's raw value")
            XCTAssertFalse(id.hasPrefix("g:") || id.hasPrefix("u:"))
        }
    }

    /// The whole drawn list, keyed the way `FleetView` keys it: one key per
    /// child, all distinct. This is the assertion the panel's viewport
    /// arithmetic actually rests on — the count of keys IS the count of gaps.
    func testEveryDrawnChildGetsItsOwnKeyOnAFleetFullOfDuplicates() {
        let fleet = Fleet(accounts: [
            sectionAccount("both@example.com", groups: ["dev", "ops"]),
            sectionAccount("spent@example.com", groups: ["dev", "ops"], quotaState: .spent),
            sectionAccount("plain@example.com", groups: nil),
        ])
        let sections = fleet.sectionsInDisplayOrder()

        var keys: [String] = []
        for (index, section) in sections.enumerated() {
            if sections.isFirstOfBand(index) { keys.append("b:\(section.band.rawValue)") }
            keys.append("h:\(section.id)")
            keys.append(contentsOf: section.rows.map(\.id))
        }

        // 2 band headings (live, out of tokens) + 5 group headings (dev, ops
        // and ungrouped in the live band; dev and ops in the spent one)
        // + 5 rows: `both` and `spent` are each drawn twice, `plain` once.
        XCTAssertEqual(keys.count, 12)
        XCTAssertEqual(Set(keys).count, keys.count, "two list children share a measured-height key")
    }
}

/// Same fixture shape as `FleetSectionsTests`, so the two files cannot drift.
private func sectionAccount(
    _ name: String,
    groups: [String]?,
    quotaState: QuotaState = .ok,
    disabled: Bool = false
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
        reservedGroups: nil,
        parkedGroups: nil,
        groupColors: nil
    )
}
