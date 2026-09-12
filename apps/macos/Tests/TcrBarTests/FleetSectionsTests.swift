import XCTest

@testable import TcrBarCore

/// `Fleet.sectionsInDisplayOrder(pinning:)` — the panel's list cut into state
/// bands, then group sections, then rows.
///
/// The two decisions under test are Gil's overrides of
/// `docs/plans/account-groups-plan.md:44-55`: real sections with DUPLICATED
/// rows (no primary group, no de-duplication), and state as the outer level so
/// a group split across states appears in two bands. Both are asserted here
/// rather than described, because both are the kind of rule a later "cleanup"
/// deletes as an apparent bug.
///
/// Account names are obviously fake — this repository is public.
final class FleetSectionsTests: XCTestCase {

    // MARK: - Duplication, which is the point

    /// An account in two groups appears under BOTH headings — two rows, one
    /// account. The override's whole cost, asserted so nobody "fixes" it.
    func testAccountInTwoGroupsAppearsUnderBoth() {
        let fleet = Fleet(accounts: [sectionAccount("both@example.com", groups: ["dev", "ops"])])
        let sections = fleet.sectionsInDisplayOrder()

        XCTAssertEqual(sections.map(\.group), [.named("dev"), .named("ops")])
        XCTAssertEqual(
            sections.flatMap { $0.rows }.map(\.account.name),
            ["both@example.com", "both@example.com"])
    }

    /// The two rows of one account carry DIFFERENT identities, because a
    /// `ForEach` identity that collides paints one row's data onto the other
    /// rather than crashing.
    func testDuplicatedRowsCarryDistinctCompositeIdentities() {
        let fleet = Fleet(accounts: [sectionAccount("both@example.com", groups: ["dev", "ops"])])
        let ids = fleet.sectionsInDisplayOrder().flatMap { $0.rows }.map(\.id)

        XCTAssertEqual(ids.count, 2)
        XCTAssertEqual(Set(ids).count, 2, "two rows of one account must not share a SwiftUI identity")
    }

    /// Every id across the whole list is unique, on a fleet that has all the
    /// collision shapes at once: multi-membership, a group named after the
    /// ungrouped heading, and two accounts in the same group.
    func testEveryRowIdentityIsUniqueAcrossTheWholeList() {
        let fleet = Fleet(accounts: [
            sectionAccount("a@example.com", groups: ["dev", "Ungrouped"]),
            sectionAccount("b@example.com", groups: ["dev"]),
            sectionAccount("c@example.com", groups: nil),
        ])
        let ids = fleet.sectionsInDisplayOrder().flatMap { $0.rows }.map(\.id)

        XCTAssertEqual(ids.count, 4)
        XCTAssertEqual(Set(ids).count, ids.count)
    }

    // MARK: - Bands

    /// Band order is live, then out of tokens, then parked — and a group whose
    /// accounts differ in state is SPLIT, appearing once per band.
    func testGroupSplitAcrossBandsAppearsInEachBandInBandOrder() {
        let fleet = Fleet(accounts: [
            sectionAccount("live@example.com", groups: ["dev"]),
            sectionAccount("spent@example.com", groups: ["dev"], quotaState: .spent),
            sectionAccount("off@example.com", groups: ["dev"], disabled: true),
        ])
        let sections = fleet.sectionsInDisplayOrder()

        XCTAssertEqual(sections.map(\.band), [.live, .outOfTokens, .parked])
        XCTAssertEqual(sections.map(\.group), [.named("dev"), .named("dev"), .named("dev")])
        XCTAssertEqual(
            sections.map { $0.rows.map(\.account.name) },
            [["live@example.com"], ["spent@example.com"], ["off@example.com"]])
    }

    /// The three sections of one split group have distinct identities too —
    /// band is part of a SECTION's id even though it is not part of a row's.
    func testSplitGroupSectionsHaveDistinctIdentities() {
        let fleet = Fleet(accounts: [
            sectionAccount("live@example.com", groups: ["dev"]),
            sectionAccount("spent@example.com", groups: ["dev"], quotaState: .spent),
            sectionAccount("off@example.com", groups: ["dev"], disabled: true),
        ])
        let ids = fleet.sectionsInDisplayOrder().map(\.id)
        XCTAssertEqual(Set(ids).count, 3)
    }

    /// A wholly-parked group — `tcr group park`, which is the live fleet's
    /// `henry-token` — lands entirely in the parked band even though no row
    /// carries `disabled`. Parking blocks the account for every request
    /// (`Manager::parked_blocks`), so filing it under "Live" would claim
    /// capacity the router will never use.
    func testWhollyParkedGroupLandsInTheParkedBand() {
        let fleet = Fleet(accounts: [
            sectionAccount("p1@example.com", groups: ["henry-token"], parkedGroups: ["henry-token"]),
            sectionAccount("p2@example.com", groups: ["henry-token"], parkedGroups: ["henry-token"]),
        ])
        let sections = fleet.sectionsInDisplayOrder()

        XCTAssertEqual(sections.count, 1)
        XCTAssertEqual(sections.first?.band, .parked)
        XCTAssertEqual(sections.first?.rows.map(\.account.name), ["p1@example.com", "p2@example.com"])
    }

    /// A disabled row is parked even when its quota says spent — the operator's
    /// decision outranks the reset countdown, exactly as `displayOrder` orders
    /// them.
    func testDisabledOutranksSpent() {
        let fleet = Fleet(accounts: [
            sectionAccount("x@example.com", groups: ["dev"], quotaState: .spent, disabled: true)
        ])
        XCTAssertEqual(fleet.sectionsInDisplayOrder().map(\.band), [.parked])
    }

    /// A never-probed row and a dead-credential row are LIVE, not spent and not
    /// parked: neither is measured as out of tokens and neither was parked by
    /// the operator.
    func testUnmeasuredAndNeedsReloginAreLive() {
        let fleet = Fleet(accounts: [
            sectionAccount("u@example.com", groups: ["dev"], quota: nil),
            sectionAccount("r@example.com", groups: ["dev"], status: "error"),
        ])
        XCTAssertEqual(fleet.sectionsInDisplayOrder().map(\.band), [.live])
        XCTAssertEqual(fleet.sectionsInDisplayOrder().first?.rows.count, 2)
    }

    // MARK: - Group keys

    /// `groups == nil` (a server too old to report the field) and `groups == []`
    /// (reported, none) both render as ungrouped — one section, not two, and no
    /// "not reported" heading nobody can act on.
    func testNilAndEmptyGroupsBothRenderUngroupedInOneSection() {
        let fleet = Fleet(accounts: [
            sectionAccount("nil@example.com", groups: nil),
            sectionAccount("empty@example.com", groups: []),
        ])
        let sections = fleet.sectionsInDisplayOrder()

        XCTAssertEqual(sections.count, 1)
        XCTAssertEqual(sections.first?.group, .ungrouped)
        XCTAssertEqual(
            sections.first?.rows.map(\.account.name),
            ["empty@example.com", "nil@example.com"])
    }

    /// A fleet with no groups at all is one ungrouped section — the panel still
    /// renders a list, it does not go empty.
    func testFleetWithNoGroupsAtAllIsOneUngroupedSection() {
        let fleet = Fleet(accounts: [
            sectionAccount("b@example.com", groups: nil),
            sectionAccount("a@example.com", groups: nil),
        ])
        let sections = fleet.sectionsInDisplayOrder()

        XCTAssertEqual(sections.map(\.group), [.ungrouped])
        XCTAssertEqual(sections.first?.rows.map(\.account.name), ["a@example.com", "b@example.com"])
        XCTAssertEqual(sections.first?.title, "Ungrouped")
    }

    /// Ungrouped sorts LAST within its band; named groups sort alphabetically
    /// regardless of the wire's array order.
    func testUngroupedSortsLastAndNamedGroupsAlphabetically() {
        let fleet = Fleet(accounts: [
            sectionAccount("z@example.com", groups: nil),
            sectionAccount("m@example.com", groups: ["ops"]),
            sectionAccount("a@example.com", groups: ["dev"]),
        ])
        XCTAssertEqual(
            fleet.sectionsInDisplayOrder().map(\.group),
            [.named("dev"), .named("ops"), .ungrouped])
    }

    /// A group actually named "Ungrouped" is a different section from the
    /// unlabelled bucket, and their rows' identities do not collide.
    func testGroupNamedUngroupedDoesNotImpersonateTheUnlabelledBucket() {
        let fleet = Fleet(accounts: [
            sectionAccount("a@example.com", groups: ["Ungrouped"]),
            sectionAccount("a@example.com", groups: nil),
        ])
        let sections = fleet.sectionsInDisplayOrder()

        XCTAssertEqual(sections.map(\.group), [.named("Ungrouped"), .ungrouped])
        XCTAssertEqual(Set(sections.flatMap { $0.rows }.map(\.id)).count, 2)
    }

    /// An empty fleet yields no sections — no empty headings.
    func testEmptyFleetYieldsNoSections() {
        XCTAssertEqual(Fleet(accounts: []).sectionsInDisplayOrder().count, 0)
    }

    // MARK: - Row ordering and the control account

    /// Within a section, accounts sort by name.
    func testRowsWithinASectionSortByName() {
        let fleet = Fleet(accounts: [
            sectionAccount("c@example.com", groups: ["dev"]),
            sectionAccount("a@example.com", groups: ["dev"]),
            sectionAccount("b@example.com", groups: ["dev"]),
        ])
        XCTAssertEqual(
            fleet.sectionsInDisplayOrder().first?.rows.map(\.account.name),
            ["a@example.com", "b@example.com", "c@example.com"])
    }

    /// The control account is pinned first inside EVERY section it appears in,
    /// and flagged — it is not hoisted above the bands, which would put it
    /// outside its own group and state.
    func testControlAccountIsPinnedFirstInEverySectionItAppearsIn() {
        let fleet = Fleet(accounts: [
            sectionAccount("a@example.com", groups: ["dev"]),
            sectionAccount("z@example.com", groups: ["dev", "ops"]),
            sectionAccount("b@example.com", groups: ["ops"]),
        ])
        let sections = fleet.sectionsInDisplayOrder(pinning: "z@example.com")

        XCTAssertEqual(
            sections.map { $0.rows.map(\.account.name) },
            [["z@example.com", "a@example.com"], ["z@example.com", "b@example.com"]])
        XCTAssertEqual(sections.map { $0.rows.map(\.isControl) }, [[true, false], [true, false]])
        XCTAssertEqual(sections.map(\.containsControl), [true, true])
    }

    /// A control account that is spent stays in its own band — the pin is
    /// within a section, never across one. State wins, which is decision two.
    func testControlAccountIsNotHoistedOutOfItsBand() {
        let fleet = Fleet(accounts: [
            sectionAccount("live@example.com", groups: ["dev"]),
            sectionAccount("ctl@example.com", groups: ["dev"], quotaState: .spent),
        ])
        let sections = fleet.sectionsInDisplayOrder(pinning: "ctl@example.com")

        XCTAssertEqual(sections.map(\.band), [.live, .outOfTokens])
        XCTAssertEqual(sections.first?.rows.map(\.account.name), ["live@example.com"])
        XCTAssertEqual(sections.last?.rows.map(\.isControl), [true])
    }

    /// A `nil` control name — none set, or a build that cannot ask — leaves
    /// everything ordered strictly by name and nothing flagged.
    func testNilControlNameLeavesPlainNameOrder() throws {
        let fleet = Fleet(accounts: [
            sectionAccount("z@example.com", groups: ["dev"]),
            sectionAccount("a@example.com", groups: ["dev"]),
        ])
        let rows = try XCTUnwrap(fleet.sectionsInDisplayOrder(pinning: nil).first?.rows)

        XCTAssertEqual(rows.map(\.account.name), ["a@example.com", "z@example.com"])
        XCTAssertEqual(rows.map(\.isControl), [false, false])
    }

    /// A control name that matches no row changes nothing — the controller can
    /// report a name this fleet no longer holds.
    func testUnknownControlNameChangesNothing() {
        let fleet = Fleet(accounts: [
            sectionAccount("z@example.com", groups: ["dev"]),
            sectionAccount("a@example.com", groups: ["dev"]),
        ])
        XCTAssertEqual(
            fleet.sectionsInDisplayOrder(pinning: "ghost@example.com").first?
                .rows.map(\.account.name), ["a@example.com", "z@example.com"])
    }

    // MARK: - Group outline (docs/plans/group-outline-bridge.md)

    /// A named group's section carries its own server-resolved colour.
    /// `.ungrouped` carries none — decision #1, its absent outline IS the
    /// signal — which is the same `nil` the next test locks down for a
    /// DIFFERENT reason (a real group whose colour never resolved).
    func testNamedGroupSectionCarriesItsColorAndUngroupedCarriesNone() {
        let fleet = Fleet(accounts: [
            sectionAccount(
                "a@example.com", groups: ["dev"], groupColors: ["dev": "#0a84ff"]),
            sectionAccount("b@example.com", groups: nil),
        ])
        let sections = fleet.sectionsInDisplayOrder()

        let dev = try? XCTUnwrap(sections.first { $0.group == .named("dev") })
        XCTAssertTrue(dev?.isOutlined ?? false)
        let devColor = dev?.outlineColor
        XCTAssertEqual(devColor?.red ?? -1, 0x0a / 255.0, accuracy: 0.001)
        XCTAssertEqual(devColor?.green ?? -1, 0x84 / 255.0, accuracy: 0.001)
        XCTAssertEqual(devColor?.blue ?? -1, 0xff / 255.0, accuracy: 0.001)

        let ungrouped = try? XCTUnwrap(sections.first { $0.group == .ungrouped })
        XCTAssertFalse(ungrouped?.isOutlined ?? true)
        XCTAssertNil(ungrouped?.outlineColor)
    }

    /// A named group with no `groupColors` entry — an older server, or the
    /// field genuinely absent — answers `isOutlined == false`, exactly like
    /// `.ungrouped` does, and the view draws NO outline for either. An
    /// earlier version of this answered `true` here and had the view fall
    /// back to a neutral box, mirroring `GroupTag`'s own colourless-chip
    /// fallback; that was reverted because an outline that is only ever a
    /// stroke has nothing left to say once its colour is gone — a chip
    /// still has its text. An absent box beats an invisible one.
    func testNamedGroupWithNoResolvedColorDrawsNoOutlineEitherJustLikeUngrouped() {
        let fleet = Fleet(accounts: [sectionAccount("a@example.com", groups: ["dev"])])
        let section = try? XCTUnwrap(fleet.sectionsInDisplayOrder().first)

        XCTAssertFalse(section?.isOutlined ?? true)
        XCTAssertNil(section?.outlineColor)
    }

    // MARK: - What the row and the view need

    /// A row's band matches the section it is in — the carried copy cannot
    /// disagree with the heading above it.
    func testRowBandMatchesItsSection() {
        let fleet = Fleet(accounts: [
            sectionAccount("a@example.com", groups: ["dev"]),
            sectionAccount("s@example.com", groups: ["dev"], quotaState: .spent),
        ])
        for section in fleet.sectionsInDisplayOrder() {
            XCTAssertTrue(section.rows.allSatisfy { $0.band == section.band && $0.group == section.group })
        }
    }

    /// `isFirstOfBand` marks exactly the sections that open a band, so the view
    /// draws one band heading per run rather than one per section.
    func testIsFirstOfBandMarksOnlyTheOpeningSectionOfEachRun() {
        let fleet = Fleet(accounts: [
            sectionAccount("a@example.com", groups: ["dev"]),
            sectionAccount("b@example.com", groups: ["ops"]),
            sectionAccount("s@example.com", groups: ["dev"], quotaState: .spent),
        ])
        let sections = fleet.sectionsInDisplayOrder()

        XCTAssertEqual(sections.count, 3)
        XCTAssertEqual(sections.indices.map { sections.isFirstOfBand($0) }, [true, false, true])
    }

    /// Band headings read as sentences an operator can act on.
    func testBandTitles() {
        XCTAssertEqual(FleetBand.live.title, "Live")
        XCTAssertEqual(FleetBand.outOfTokens.title, "Out of tokens")
        XCTAssertEqual(FleetBand.parked.title, "Parked")
    }

    /// A lone "Ungrouped" heading under a band heading says nothing the band
    /// heading did not, and costs the viewport its own height plus a gap. On a
    /// fleet with no groups configured that was EVERY band — three dead rows
    /// saying nothing three times. Caught by rendering the panel and looking at
    /// the PNG, not by any test, which is why this one exists.
    func testALoneUngroupedSectionDrawsNoGroupHeading() {
        let fleet = Fleet(accounts: [
            sectionAccount("a@example.com", groups: []),
            sectionAccount("b@example.com", groups: []),
        ])
        let sections = fleet.sectionsInDisplayOrder()
        XCTAssertEqual(sections.count, 1, "no groups configured — one ungrouped section")
        XCTAssertFalse(
            sections.drawsGroupHeading(at: 0),
            "the only section in its band, and ungrouped — the heading is dead space")
    }

    /// A lone NAMED section still draws: "PARKED / henry-team" says WHICH group
    /// was parked, which the band heading cannot.
    func testALoneNamedSectionStillDrawsItsHeading() {
        let fleet = Fleet(accounts: [sectionAccount("a@example.com", groups: ["dev"])])
        let sections = fleet.sectionsInDisplayOrder()
        XCTAssertEqual(sections.count, 1)
        XCTAssertTrue(
            sections.drawsGroupHeading(at: 0),
            "a named group is information the band heading does not carry")
    }

    /// Ungrouped alongside a named section DOES draw — there it is the thing
    /// separating the two.
    func testUngroupedDrawsWhenItSharesABandWithANamedSection() {
        let fleet = Fleet(accounts: [
            sectionAccount("a@example.com", groups: ["dev"]),
            sectionAccount("b@example.com", groups: []),
        ])
        let sections = fleet.sectionsInDisplayOrder()
        XCTAssertEqual(sections.count, 2)
        for index in sections.indices {
            XCTAssertTrue(
                sections.drawsGroupHeading(at: index),
                "two sections in one band — both headings separate something")
        }
    }
}

/// A row with everything but the group/state fields fixed — the same shape
/// `GroupTagTests` uses, so the two files' fixtures cannot drift.
private func sectionAccount(
    _ name: String,
    groups: [String]?,
    parkedGroups: [String]? = nil,
    quotaState: QuotaState = .ok,
    quota: Double? = 0,
    status: String = "active",
    disabled: Bool = false,
    groupColors: [String: String]? = nil
) -> Account {
    Account(
        name: name,
        priority: 1,
        status: status,
        disabled: disabled,
        quota: quota,
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
        parkedGroups: parkedGroups,
        groupColors: groupColors
    )
}
