import XCTest

@testable import TcrBarCore

/// `Account.groupTags` and `GroupTagColor` — the small colored tag on an
/// account row that is now the entire group-membership UI (bridge:
/// `docs/plans/group-tags-bridge.md`). Account names are obviously fake —
/// this repository is public.
final class GroupTagTests: XCTestCase {

    // MARK: - Account.groupTags

    /// An account in two groups yields two tags, alphabetically ordered
    /// regardless of the wire's own array order — the same stability
    /// ``Account/groupMenuActions`` already gives its removal entries.
    func testAccountInTwoGroupsYieldsTwoTagsInStableOrder() {
        let account = tagAccount("a@example.com", groups: ["dev", "codereview"])
        XCTAssertEqual(account.groupTags.map(\.name), ["codereview", "dev"])
    }

    /// An ungrouped account yields no tags at all — no "ungrouped" tag, no
    /// empty space reserved, per the bridge.
    func testUngroupedAccountYieldsNoTags() {
        XCTAssertEqual(tagAccount("a@example.com", groups: nil).groupTags, [])
        XCTAssertEqual(tagAccount("a@example.com", groups: []).groupTags, [])
    }

    /// A group with no wire color (missing from `groupColors`, or an older
    /// server that never sent the field at all) falls back to `nil` — the
    /// view's cue to draw the neutral token rather than guess.
    func testGroupWithNoWireColorFallsBackToNilBackground() {
        let missingKey = tagAccount(
            "a@example.com", groups: ["dev"], groupColors: ["codereview": "#32d74b"])
        XCTAssertNil(missingKey.groupTags.first?.background)

        let noFieldAtAll = tagAccount("a@example.com", groups: ["dev"], groupColors: nil)
        XCTAssertNil(noFieldAtAll.groupTags.first?.background)
    }

    /// A malformed hex value falls back to `nil` rather than throwing or
    /// producing a garbage color.
    func testMalformedHexFallsBackRatherThanThrowing() {
        let account = tagAccount(
            "a@example.com", groups: ["dev"], groupColors: ["dev": "not-a-color"])
        XCTAssertNil(account.groupTags.first?.background)
    }

    /// A well-formed hex resolves to the exact channel values, never
    /// derived client-side — the server's own resolution, decoded verbatim.
    func testWellFormedHexResolvesToExactChannels() {
        let account = tagAccount(
            "a@example.com", groups: ["dev"], groupColors: ["dev": "#32d74b"])
        let rgb = try! XCTUnwrap(account.groupTags.first?.background)
        XCTAssertEqual(rgb.red, 0x32 / 255.0, accuracy: 0.0001)
        XCTAssertEqual(rgb.green, 0xd7 / 255.0, accuracy: 0.0001)
        XCTAssertEqual(rgb.blue, 0x4b / 255.0, accuracy: 0.0001)
    }

    /// A reserved group is distinguishable from a plain one via
    /// `isReserved`, a fact independent of `background` — color already
    /// carries the group's identity, so the reserved cue must not also ride
    /// color.
    func testReservedGroupIsMarkedIndependentlyOfColor() {
        let account = tagAccount(
            "a@example.com",
            groups: ["dev", "codereview"],
            reservedGroups: ["codereview"]
        )
        let byName = Dictionary(uniqueKeysWithValues: account.groupTags.map { ($0.name, $0) })
        XCTAssertEqual(byName["codereview"]?.isReserved, true)
        XCTAssertEqual(byName["dev"]?.isReserved, false)
    }

    /// `nil` `reservedGroups` (an older server) degrades to nothing
    /// reserved, same as `groups` itself degrading to ungrouped.
    func testNilReservedGroupsDegradesToNothingReserved() {
        let account = tagAccount("a@example.com", groups: ["dev"], reservedGroups: nil)
        XCTAssertEqual(account.groupTags.first?.isReserved, false)
    }

    // MARK: - Account.servesGroupTrafficOnly (the "group only" row state)

    /// An account whose only group is reserved serves no pool traffic, so the
    /// row must not also claim to be "rotating" — the `GIL` + `ROTATING` pair
    /// that read as "tagged AND pooled" and hid a real leak.
    func testReservedGroupMakesAccountGroupOnly() {
        let account = tagAccount(
            "a@example.com", groups: ["codereview"], reservedGroups: ["codereview"])
        XCTAssertTrue(account.servesGroupTrafficOnly)
    }

    /// The case an `allSatisfy` implementation gets wrong. The server holds an
    /// account out of general rotation when ANY group is reserved, so one
    /// reserved group plus one plain group is still group-only.
    func testOneReservedGroupAmongPlainOnesIsStillGroupOnly() {
        let account = tagAccount(
            "a@example.com",
            groups: ["dev", "codereview"],
            reservedGroups: ["codereview"]
        )
        XCTAssertTrue(
            account.servesGroupTrafficOnly,
            "any reserved group holds the account out of the pool, not only all of them"
        )
    }

    /// Tagged but unreserved is the silent no-op: the account still takes pool
    /// traffic, so it genuinely is rotating and must keep saying so.
    func testTaggedButUnreservedAccountStillRotates() {
        let account = tagAccount("a@example.com", groups: ["dev"], reservedGroups: [])
        XCTAssertFalse(account.servesGroupTrafficOnly)
    }

    /// An ungrouped account is plain rotation — no groups, nothing reserved.
    func testUngroupedAccountIsNotGroupOnly() {
        XCTAssertFalse(tagAccount("a@example.com", groups: nil).servesGroupTrafficOnly)
    }

    // MARK: - GroupTagColor

    func testMalformedHexReturnsNilFromParse() {
        XCTAssertNil(GroupTagColor.parse("not-a-color"))
        XCTAssertNil(GroupTagColor.parse("#zzzzzz"), "not valid hex digits")
        XCTAssertNil(GroupTagColor.parse("#fff"), "must be exactly 6 digits, not 3")
        XCTAssertNil(GroupTagColor.parse("32d74b"), "must carry the leading #")
    }

    func testWellFormedHexParsesToChannels() {
        let rgb = try! XCTUnwrap(GroupTagColor.parse("#32d74b"))
        XCTAssertEqual(rgb.red, 0x32 / 255.0, accuracy: 0.0001)
        XCTAssertEqual(rgb.green, 0xd7 / 255.0, accuracy: 0.0001)
        XCTAssertEqual(rgb.blue, 0x4b / 255.0, accuracy: 0.0001)
    }

    /// The foreground choice is luminance-driven, not a fixed pick: a dark
    /// background wants light text, a light background wants dark text.
    /// Two samples each way, so this is not just checking one boundary.
    func testForegroundIsChosenByLuminanceLightOnDarkAndDarkOnLight() {
        let black = try! XCTUnwrap(GroupTagColor.parse("#000000"))
        XCTAssertFalse(GroupTagColor.isLight(black), "black needs light (white) text")

        let darkBlue = try! XCTUnwrap(GroupTagColor.parse("#0a2472"))
        XCTAssertFalse(GroupTagColor.isLight(darkBlue), "a dark blue also needs light text")

        let white = try! XCTUnwrap(GroupTagColor.parse("#ffffff"))
        XCTAssertTrue(GroupTagColor.isLight(white), "white needs dark (black) text")

        let paleYellow = try! XCTUnwrap(GroupTagColor.parse("#fdf5b0"))
        XCTAssertTrue(GroupTagColor.isLight(paleYellow), "a pale yellow also needs dark text")
    }
}

/// Hand-built accounts with the fields this file's assertions touch.
private func tagAccount(
    _ name: String,
    groups: [String]?,
    reservedGroups: [String]? = nil,
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
        groupColors: groupColors
    )
}
