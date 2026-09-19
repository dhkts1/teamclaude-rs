import XCTest

@testable import TcrBarCore

/// "Exits from" on an account card, its four states, and the argv behind the
/// picker and the must switch.
final class AccountExitTests: XCTestCase {

    // MARK: Parsing the file's own two keys

    func testTheRouteIsParsedFromTheConfigsOwnSpelling() {
        XCTAssertEqual(AccountExit.Route(egress: "local"), .local)
        XCTAssertEqual(AccountExit.Route(egress: nil), .local)
        XCTAssertEqual(AccountExit.Route(egress: "via studio-mac"), .via("studio-mac"))
    }

    /// A spelling this build cannot read keeps its own words. Calling it local
    /// would report that traffic leaves from THIS Mac when the file says
    /// something else, which is the one claim this control must never get
    /// wrong.
    func testAnUnreadableRouteIsNeverCalledLocal() {
        XCTAssertEqual(AccountExit.Route(egress: "through-the-moon"), .unknown("through-the-moon"))
        XCTAssertEqual(AccountExit.Route(egress: "via "), .unknown("via"))
        XCTAssertNil(AccountExit.Route(egress: "through-the-moon").peer)
    }

    func testTheWireDecodesBothKeysAndTheLiveFacts() throws {
        let json = """
            {"egress":"via studio-mac","egressStrict":true,"peerDown":true,
             "waitingSeconds":40}
            """
        let exit = try JSONDecoder().decode(AccountExit.self, from: Data(json.utf8))
        XCTAssertEqual(exit.route, .via("studio-mac"))
        XCTAssertTrue(exit.strict)
        XCTAssertTrue(exit.peerDown)
        XCTAssertEqual(exit.waitingSeconds, 40)
    }

    /// The document's `exits` map is EMPTY against every `tcr` in this tree,
    /// and empty draws no row: a picker reading "This Mac" for an account
    /// nobody reported on would be a claim, not a readout.
    func testAnAbsentExitsMapLeavesEveryAccountWithoutARow() throws {
        let document = try JSONDecoder().decode(
            PeerListDocument.self, from: Data("{\"supported\":true}".utf8))
        XCTAssertTrue(document.exits.isEmpty)
        XCTAssertNil(PeerLease.exit(forAccountLabel: "alice", in: document.exits))
    }

    /// The masked key names several accounts and identifies none, so it
    /// matches nothing, exactly as the lent-to lookup already refuses it.
    func testAMaskedLabelMatchesNoExit() {
        let exits = [PeerLease.maskedLabel: AccountExit(route: .via("studio-mac"))]
        XCTAssertNil(PeerLease.exit(forAccountLabel: PeerLease.maskedLabel, in: exits))
    }

    // MARK: The four states

    func testLocalShowsNoMustControlAndNoNote() {
        let exit = AccountExit(route: .local)
        XCTAssertFalse(exit.showsMust)
        XCTAssertNil(exit.note)
        XCTAssertNil(exit.waitingPill)
        XCTAssertEqual(exit.route.label, "This Mac")
    }

    /// Soft: the note says what silently falling back actually costs, because
    /// the whole reason to pin an exit is to keep one address.
    func testSoftNamesTheCostOfFallingBack() {
        let exit = AccountExit(route: .via("studio-mac"))
        XCTAssertTrue(exit.showsMust)
        XCTAssertFalse(exit.noteIsWarning)
        XCTAssertEqual(exit.note?.contains("use this Mac instead"), true)
        XCTAssertEqual(exit.note?.contains("can change without warning"), true)
        XCTAssertNil(exit.waitingPill)
    }

    /// Must: the note states the real behaviour, refusing rather than moving.
    /// A screen that says "must" and then quietly reroutes has lied.
    func testMustSaysTheyFailRatherThanReroute() {
        let exit = AccountExit(route: .via("studio-mac"), strict: true)
        XCTAssertTrue(exit.noteIsWarning)
        XCTAssertEqual(exit.note?.contains("They fail, not reroute"), true)
        XCTAssertNil(exit.waitingPill, "nothing is waiting until the Mac is actually down")
    }

    /// THE state the lead ruled in: must, and the exit Mac is down. The
    /// readout names the Mac, and the note reports what is happening now
    /// rather than the hypothetical.
    func testMustWithThePeerDownNamesTheMacAndReportsTheWait() {
        let exit = AccountExit(
            route: .via("studio-mac"), strict: true, peerDown: true, waitingSeconds: 40)
        XCTAssertEqual(exit.waitingPill, "waiting for studio-mac")
        XCTAssertEqual(exit.note?.hasPrefix("studio-mac is down."), true)
        XCTAssertEqual(exit.note?.contains("Waiting 40s."), true)
        XCTAssertTrue(exit.noteIsWarning)
    }

    /// A down Mac with no reported wait says everything except a figure it
    /// never measured.
    func testADownPeerWithNoReportedWaitPrintsNoFigure() {
        let exit = AccountExit(route: .via("studio-mac"), strict: true, peerDown: true)
        XCTAssertEqual(exit.note?.contains("Waiting"), false)
        XCTAssertEqual(exit.waitingPill, "waiting for studio-mac")
    }

    /// A soft account whose Mac is down is NOT waiting: its requests moved to
    /// this Mac, which is exactly what soft means.
    func testASoftAccountIsNeverWaiting() {
        let exit = AccountExit(route: .via("studio-mac"), peerDown: true)
        XCTAssertNil(exit.waitingPill)
    }

    // MARK: Resolving the wire id through `peers`

    /// The picker's actual read: `.via` holds the WIRE id, and
    /// `label(peers:)` joins it to the row's name through `peers`: the
    /// `PeerListDocument` this route came from.
    func testLabelResolvesTheWireIdToThePeersRowName() {
        let route = AccountExit.Route.via("0W3GE1R70W3GE1R70W3GE1R70W3GE1R70W3GE1R70W3GE1R70W3G")
        let peers = [
            PeerListDocument.PeerEntry(
                id: "0W3GE1R70W3GE1R70W3GE1R70W3GE1R70W3GE1R70W3GE1R70W3G", name: "studio-mac")
        ]
        XCTAssertEqual(route.label(peers: peers), "studio-mac")
        // `label` alone (no document) still carries the wire id; it cannot
        // resolve one with nothing to resolve it against.
        XCTAssertEqual(route.label, "0W3GE1R70W3GE1R70W3GE1R70W3GE1R70W3GE1R70W3GE1R70W3G")
    }

    /// No row in `peers` matches the id: the masked short form, NEVER the
    /// raw 52-character wire id, since this repo is public.
    func testLabelFallsBackToTheMaskedIdWhenNoRowMatches() {
        let route = AccountExit.Route.via("0W3GE1R70W3GE1R70W3GE1R70W3GE1R70W3GE1R70W3GE1R70W3G")
        XCTAssertEqual(route.label(peers: []), "tcr-0W3GE1R70W")
    }

    /// A row matches the id but carries no name yet (Settings > Peers reads
    /// `name` as optional). Same fallback: a masked id, not a raw one and not
    /// a blank line.
    func testLabelFallsBackToTheMaskedIdWhenTheMatchedRowHasNoNameYet() {
        let route = AccountExit.Route.via("0W3GE1R70W3GE1R70W3GE1R70W3GE1R70W3GE1R70W3GE1R70W3G")
        let peers = [
            PeerListDocument.PeerEntry(
                id: "0W3GE1R70W3GE1R70W3GE1R70W3GE1R70W3GE1R70W3GE1R70W3G", name: nil)
        ]
        XCTAssertEqual(route.label(peers: peers), "tcr-0W3GE1R70W")
    }

    /// `.local` and `.unknown` need no document at all: `label(peers:)`
    /// agrees with `label` for both.
    func testLabelPeersAgreesWithLabelForLocalAndUnknown() {
        XCTAssertEqual(AccountExit.Route.local.label(peers: []), "This Mac")
        XCTAssertEqual(
            AccountExit.Route.unknown("through-the-moon").label(peers: []), "through-the-moon")
    }

    /// `note(peers:)` and `waitingPill(peers:)` carry the same resolved name
    /// as `label(peers:)`, never the wire id: the doc/behaviour mismatch the
    /// second review found.
    func testNoteAndWaitingPillResolveThroughPeersToo() {
        let id = "0W3GE1R70W3GE1R70W3GE1R70W3GE1R70W3GE1R70W3GE1R70W3G"
        let peers = [PeerListDocument.PeerEntry(id: id, name: "studio-mac")]
        let exit = AccountExit(route: .via(id), strict: true, peerDown: true, waitingSeconds: 40)
        XCTAssertEqual(exit.waitingPill(peers: peers), "waiting for studio-mac")
        XCTAssertEqual(exit.note(peers: peers)?.hasPrefix("studio-mac is down."), true)
        // No document: the wire-id variants still carry the raw id, same
        // caveat as `label`.
        XCTAssertEqual(exit.waitingPill, "waiting for \(id)")
        XCTAssertEqual(exit.note?.hasPrefix("\(id) is down."), true)
    }

    // MARK: The must control's own label

    /// The switch names what it does: `must use <peer>`, not a bare "must"
    /// with no verb and no object.
    func testMustLabelNamesTheChosenPeer() {
        let exit = AccountExit(route: .via("studio-mac"), strict: true)
        XCTAssertEqual(exit.mustLabel, "must use studio-mac")
    }

    /// The resolved form reads the peer's name through `peers`, the same way
    /// every other readout on this row does, never the wire id.
    func testMustLabelPeersResolvesTheNameThroughPeers() {
        let id = "0W3GE1R70W3GE1R70W3GE1R70W3GE1R70W3GE1R70W3GE1R70W3G"
        let peers = [PeerListDocument.PeerEntry(id: id, name: "studio-mac")]
        let exit = AccountExit(route: .via(id), strict: true)
        XCTAssertEqual(exit.mustLabel(peers: peers), "must use studio-mac")
        // No document: the wire-id variant still carries the raw id, same
        // caveat as `label`.
        XCTAssertEqual(exit.mustLabel, "must use \(id)")
    }

    // MARK: Argv

    func testTheArgvIsTheStateEachControlMovesTo() {
        XCTAssertEqual(
            PeerCommand.accountExit(account: "alice", route: .via("studio-mac"), must: true),
            ["peer", "account", "alice", "--exits-from", "studio-mac", "--must"])
        XCTAssertEqual(
            PeerCommand.accountExit(account: "alice", route: .via("studio-mac"), must: false),
            ["peer", "account", "alice", "--exits-from", "studio-mac", "--no-must"])
        XCTAssertEqual(
            PeerCommand.accountExit(account: "alice", route: .local, must: false),
            ["peer", "account", "alice", "--exits-from", "local", "--no-must"])
    }

    // MARK: Words

    /// The lead's ruling: the field is called `egress` in the file and that
    /// word never reaches this screen, because the same tab strip already uses
    /// it for a different control.
    func testTheWordEgressNeverReachesTheScreen() {
        let strings: [String] = [
            AccountExit(route: .local).route.label,
            AccountExit(route: .via("studio-mac")).note ?? "",
            AccountExit(route: .via("studio-mac"), strict: true).note ?? "",
            AccountExit(
                route: .via("studio-mac"), strict: true, peerDown: true, waitingSeconds: 40
            ).note ?? "",
            AccountExit(
                route: .via("studio-mac"), strict: true, peerDown: true
            ).waitingPill ?? "",
        ]
        for text in strings {
            XCTAssertFalse(text.isEmpty)
            for word in ["egress", "IK", "NAT-PMP", "UPnP"] {
                XCTAssertFalse(text.contains(word), "\(word) reached: \(text)")
            }
        }
    }
}
