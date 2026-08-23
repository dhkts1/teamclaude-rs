import XCTest

@testable import TcrBarCore

/// ``GroupRouting/routes(group:accounts:controlName:)`` — the panel's copy of a
/// proxy rule: an inference pick never selects the control account, so a group
/// with no other member serves nothing.
///
/// The shape these guard is one that shipped on a live fleet and looked healthy
/// from every surface — one member, a colour, an `active` row — while every
/// request asking for the group was served from the whole pool instead.
///
/// Account names are obviously fake — this repository is public.
final class GroupRoutingTests: XCTestCase {

    /// The defect, on the panel side. `research` has exactly one member and it
    /// is the control account.
    func testAGroupHoldingOnlyTheControlAccountDoesNotRoute() {
        let accounts = [
            routingAccount("gil@example.com", groups: ["research"]),
            routingAccount("worker@example.com", groups: ["dev"]),
        ]
        XCTAssertFalse(
            GroupRouting.routes(
                group: "research", accounts: accounts, controlName: "gil@example.com"))
    }

    /// The remedy the warning points at: one ordinary account joining the group
    /// makes it routable again. Without this, the check could be a constant
    /// `false` for any group the control account is in and still pass the test
    /// above.
    func testASecondOrdinaryMemberMakesTheGroupRoute() {
        let accounts = [
            routingAccount("gil@example.com", groups: ["research"]),
            routingAccount("worker@example.com", groups: ["research"]),
        ]
        XCTAssertTrue(
            GroupRouting.routes(
                group: "research", accounts: accounts, controlName: "gil@example.com"))
    }

    /// An ordinary group is never flagged — the other direction of the same
    /// control, so neither a constant `true` nor a constant `false` survives.
    func testAnOrdinaryGroupRoutes() {
        let accounts = [
            routingAccount("a@example.com", groups: ["dev"]),
            routingAccount("b@example.com", groups: ["dev"]),
        ]
        XCTAssertTrue(
            GroupRouting.routes(group: "dev", accounts: accounts, controlName: "gil@example.com"))
    }

    /// No control account set (or the panel could not resolve one): membership
    /// alone decides, and the same fleet that failed above now routes. The panel
    /// must not invent a block it cannot see.
    func testWithNoControlAccountMembershipAloneDecides() {
        let accounts = [routingAccount("gil@example.com", groups: ["research"])]
        XCTAssertTrue(
            GroupRouting.routes(group: "research", accounts: accounts, controlName: nil))
    }

    /// A label nobody carries routes nothing — there is no member to serve it.
    func testALabelNoAccountCarriesDoesNotRoute() {
        let accounts = [routingAccount("a@example.com", groups: ["dev"])]
        XCTAssertFalse(
            GroupRouting.routes(group: "resarch", accounts: accounts, controlName: nil))
    }
}

/// Hand-built accounts with the fields this file's assertions touch.
private func routingAccount(_ name: String, groups: [String]?) -> Account {
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
        groups: groups
    )
}
