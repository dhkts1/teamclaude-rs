import XCTest

@testable import TcrBarCore

/// What the v4 account card is allowed to claim about an account.
///
/// The card draws two pills from two model properties and nothing else:
/// ``FleetTally/Kind/init(account:)`` for the state pill and
/// ``Account/rotationLabel`` for the one beside it. Both used to overclaim, in
/// ways the fleet's own summary then contradicted:
///
///  - Inside one box legended PARKED, two equally idle accounts were labelled
///    `OK` and `PARKED`, and the headline counted the `OK` one as capacity.
///  - A card read `ROTATING` next to `NEEDS RE-LOGIN` — "traffic is landing
///    here" over an account serving none.
///  - An account Anthropic itself had rejected drew a lone green `OK`: the
///    tally had no bucket for the gate the pre-v4 row has drawn for months.
///
/// Every assertion below is a spoken or printed STRING, because that is the
/// surface: `swift test` links `TcrBarCore` only, so the card itself cannot be
/// instantiated here — but every word it draws comes from these two properties
/// and they can.
///
/// Account names are obviously fake; this repository is public.
final class AccountCardStateTests: XCTestCase {

    // MARK: - The rotation pill (review #7)

    /// The state is real; the pill is not. A healthy ungrouped account IS
    /// rotating and every exclusion is already `nil`, so drawing the word here
    /// put it on every healthy card at once, where it separated none of them
    /// and spent a pill's width on the card's only identifying line.
    func testAHealthyUngroupedAccountRotatesAndSaysSoWithNoPill() {
        let healthy = stateAccount("alice@example.com")
        XCTAssertEqual(healthy.rotation, .rotating)
        XCTAssertNil(
            healthy.rotationLabel,
            "the ordinary rotating account draws no pool pill; a card with no pool word IS "
                + "the card the pool is sending traffic to")
        XCTAssertNil(
            healthy.rotationHelp,
            "a sentence behind a pill nothing draws is a sentence no hover can reach")
    }

    /// The other half of the same rule, in one test so the two cannot be
    /// updated apart: the rare state keeps its word.
    func testTheReservedStateKeepsItsPillWhileTheRotatingOneDoesNot() {
        let reserved = stateAccount(
            "irene@example.com", groups: ["research"], reservedGroups: ["research"])
        XCTAssertNil(stateAccount("ivan@example.com").rotationLabel)
        XCTAssertEqual(reserved.rotationLabel, "Group only")
    }

    func testADeadCredentialNeverClaimsToBeRotating() {
        let broken = stateAccount("bob@example.com", status: "error")
        XCTAssertEqual(broken.health, .needsRelogin)
        XCTAssertNil(
            broken.rotationLabel,
            "the card drew ROTATING beside NEEDS RE-LOGIN, which reads as "
                + "\"traffic is landing here\" over an account serving none")
    }

    func testAnAccountRejectedByAnthropicNeverClaimsToBeRotating() {
        let rejected = stateAccount("carol@example.com", gate: .rejected)
        XCTAssertTrue(rejected.isRejected)
        XCTAssertNil(rejected.rotationLabel)
    }

    func testAParkedGroupMemberNeverClaimsToBeRotating() {
        let parked = stateAccount(
            "dave@example.com", groups: ["codereview"], parkedGroups: ["codereview"])
        XCTAssertTrue(parked.isParkedByGroup)
        XCTAssertNil(parked.rotationLabel)
    }

    func testAReservedAccountSaysGroupOnlyRatherThanRotating() {
        let reserved = stateAccount(
            "erin@example.com", groups: ["codereview"], reservedGroups: ["codereview"])
        XCTAssertTrue(reserved.servesGroupTrafficOnly)
        XCTAssertEqual(
            reserved.rotationLabel, "Group only",
            "a reserved account serves no pool traffic; the pre-v4 row has said "
                + "\"group only\" here since the reserved state shipped")
    }

    func testADisabledAccountDrawsNoRotationPill() {
        XCTAssertNil(stateAccount("frank@example.com", disabled: true).rotationLabel)
    }

    /// The two states stay distinguishable as CASES even though only one of
    /// them has a word: a caller that needs to know whether the pool is
    /// sending traffic asks `rotation`, never the pill string it used to
    /// compare against.
    func testTheTwoRotationStatesAreDistinguishableWithoutReadingTheirWords() {
        let reserved = stateAccount(
            "gwen@example.com", groups: ["research"], reservedGroups: ["research"])
        XCTAssertEqual(reserved.rotation, .groupOnly)
        XCTAssertEqual(stateAccount("hal@example.com").rotation, .rotating)
        XCTAssertNil(RotationState.rotating.label)
        XCTAssertEqual(RotationState.groupOnly.label, "Group only")
    }

    // MARK: - The state pill (review #7, #8)

    func testAParkedGroupMemberIsParkedNotOK() {
        let parked = stateAccount(
            "gina@example.com", groups: ["codereview"], parkedGroups: ["codereview"],
            quota: 0.1, probeStatus: .ok)
        XCTAssertEqual(
            FleetTally.Kind(account: parked), .disabled,
            "a member of a parked group serves nothing; drawing OK over it is "
                + "the capacity overclaim the PARKED legend contradicts")
        XCTAssertEqual(FleetTally.Kind(account: parked).token, "disabled")
    }

    func testARejectedAccountIsItsOwnBucketRatherThanOK() {
        let rejected = stateAccount(
            "hank@example.com", gate: .rejected, quota: 0.1, probeStatus: .ok)
        XCTAssertEqual(
            FleetTally.Kind(account: rejected), .rejected,
            "a rejected account keeps its last-learned .ok quota state, so "
                + "without its own bucket it draws a lone green OK")
        XCTAssertEqual(FleetTally.Kind(account: rejected).token, "rejected")
        XCTAssertEqual(FleetTally.Kind(account: rejected).phrase, "rejected by Anthropic")
    }

    func testADeadCredentialStillOutranksAParkedGroupsQuotaCases() {
        let broken = stateAccount("iris@example.com", status: "error", quota: 0.1, probeStatus: .ok)
        XCTAssertEqual(FleetTally.Kind(account: broken), .needsRelogin)
    }

    func testAnOperatorDisabledAccountIsStillParked() {
        XCTAssertEqual(
            FleetTally.Kind(account: stateAccount("jane@example.com", disabled: true)), .disabled)
    }

    // MARK: - The summary counts the same buckets the pills draw (review #8)

    /// The fleet in finding #8: four accounts, two of which can serve nothing.
    /// The summary used to read "3 ready · 1 parked" because the loop ran over
    /// `enabledAccounts` and then ASSIGNED `counts[.disabled]`, overwriting
    /// whatever the classifier had put there.
    func testSentenceBreakdownCountsAParkedGroupMemberAsParked() {
        let fleet = Fleet(accounts: [
            stateAccount("ken@example.com", quota: 0.1, probeStatus: .ok),
            stateAccount("lena@example.com", quota: 0.1, probeStatus: .ok),
            stateAccount("mia@example.com", disabled: true),
            stateAccount(
                "nick@example.com", groups: ["codereview"], parkedGroups: ["codereview"],
                quota: 0.1, probeStatus: .ok),
        ])
        XCTAssertEqual(
            fleet.sentenceBreakdown.map(\.sentenceLabel), ["2 ready", "2 parked"],
            "two of these four serve nothing; the line used to claim three ready")
        XCTAssertEqual(
            fleet.sentenceBreakdown.map(\.count).reduce(0, +), fleet.accounts.count,
            "every account lands in exactly one bucket, so the buckets sum")
    }

    func testBreakdownCountsAParkedGroupMemberAsParked() {
        let fleet = Fleet(accounts: [
            stateAccount("opal@example.com", quota: 0.1, probeStatus: .ok),
            stateAccount(
                "pete@example.com", groups: ["codereview"], parkedGroups: ["codereview"],
                quota: 0.1, probeStatus: .ok),
        ])
        XCTAssertEqual(fleet.breakdownLabel, "1 ok · 1 disabled")
    }

    func testSentenceBreakdownNamesARejectedAccount() {
        let fleet = Fleet(accounts: [
            stateAccount("quinn@example.com", quota: 0.1, probeStatus: .ok),
            stateAccount("rita@example.com", gate: .rejected, quota: 0.1, probeStatus: .ok),
        ])
        XCTAssertEqual(
            fleet.sentenceBreakdown.map(\.sentenceLabel), ["1 ready", "1 rejected by Anthropic"],
            "the rejected account used to be counted as the second ready one")
    }

    /// A section's own tally is the collapsed group's only visible content, so
    /// it runs through the same classifier — never a second one.
    func testASectionTallyUsesTheSameClassifier() {
        let fleet = Fleet(accounts: [
            stateAccount(
                "sam@example.com", groups: ["codereview"], parkedGroups: ["codereview"],
                quota: 0.1, probeStatus: .ok),
            stateAccount(
                "tess@example.com", groups: ["codereview"], parkedGroups: ["codereview"],
                quota: 0.1, probeStatus: .ok),
        ])
        let sections = fleet.sectionsInDisplayOrder()
        XCTAssertEqual(
            sections.first { $0.title == "codereview" }?.breakdown.map(\.label),
            ["2 disabled"])
    }
}

private func stateAccount(
    _ name: String,
    status: String = "active",
    disabled: Bool = false,
    groups: [String]? = nil,
    reservedGroups: [String]? = nil,
    parkedGroups: [String]? = nil,
    gate: GateReason? = nil,
    quota: Double? = nil,
    probeStatus: ProbeState = .never
) -> Account {
    Account(
        name: name,
        priority: 1,
        status: status,
        disabled: disabled,
        quota: quota,
        quotaState: .ok,
        fiveHour: quota,
        sevenDay: quota,
        sevenDayOi: nil,
        held: [],
        requests: 0,
        inputTokens: 0,
        outputTokens: 0,
        cacheReadTokens: 0,
        cacheHitRatio: nil,
        probeStatus: probeStatus,
        probeError: nil,
        lastStreamError: nil,
        streamErrorCount: 0,
        source: .live,
        serverSha: "abc1234",
        serverDirty: false,
        groups: groups,
        reservedGroups: reservedGroups,
        parkedGroups: parkedGroups,
        gate: gate
    )
}
