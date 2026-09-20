import XCTest

@testable import TcrBarCore

/// The banner's five rules, as arithmetic.
///
/// A notification is the one surface in this app that can interrupt somebody,
/// and every rule here is one that is easy to get wrong in a way no green
/// build shows: a banner per poll, a banner per instance id, three banners on
/// relaunch, silence forever after one Ignore. So the decision is a value with
/// no `UNUserNotificationCenter` in it and it is driven directly.
///
/// Addresses are documentation range: this repository is public.
final class KnockAnnouncerTests: XCTestCase {

    private func knock(_ address: String, name: String? = nil, id: String = "8f2c1ad63b0e4471")
        -> PeerKnock
    {
        PeerKnock(
            addr: address, instanceId: id, proposedName: name, wireVersion: 1,
            firstSeenMs: 1_700_000_000_000, lastSeenMs: 1_700_000_030_000)
    }

    /// The first read after launch is adopted SILENTLY. A launch that fired
    /// three banners for requests from twenty minutes ago teaches a person to
    /// ignore the fourth; the bar mark covers that case and is unconditional.
    func testTheFirstReadAfterLaunchIsAdoptedWithoutABanner() {
        var announcer = KnockAnnouncer()
        XCTAssertNil(
            announcer.announce(
                knocks: [knock("192.0.2.24", name: "loft-mini")], panelIsOpen: false))
        XCTAssertTrue(announcer.hasAnnounced("192.0.2.24"))
        XCTAssertNil(
            announcer.announce(
                knocks: [knock("192.0.2.24", name: "loft-mini")], panelIsOpen: false),
            "the adopted Mac rang on the very next read")
    }

    /// A Mac that arrives after the first read rings exactly once, however
    /// many reads it then stands through.
    func testANewMacRingsOnceAndThenNeverAgainWhileItStands() {
        var announcer = KnockAnnouncer()
        _ = announcer.announce(knocks: [], panelIsOpen: false)  // adoption: nobody
        let first = announcer.announce(
            knocks: [knock("192.0.2.24", name: "loft-mini")], panelIsOpen: false)
        XCTAssertEqual(first?.title, "loft-mini wants to connect")
        XCTAssertEqual(
            first?.body,
            "192.0.2.24 · expires in ten minutes. Nothing is shared until you accept and "
                + "both screens show the same six digits.")
        for poll in 1...5 {
            XCTAssertNil(
                announcer.announce(
                    knocks: [knock("192.0.2.24", name: "loft-mini")], panelIsOpen: false),
                "it rang again on poll \(poll), which at three seconds is twenty banners a "
                    + "minute for one Mac")
        }
    }

    /// The key is the ADDRESS, never the instance id. A Mac that knocks again
    /// gets a new id on the same row, so keying on the id would ring at the
    /// knock rate cap for one Mac that has asked once.
    func testTheSameAddressUnderANewInstanceIdIsSilent() {
        var announcer = KnockAnnouncer()
        _ = announcer.announce(knocks: [], panelIsOpen: false)
        _ = announcer.announce(knocks: [knock("192.0.2.24", id: "aaaaaaaaaaaaaaaa")], panelIsOpen: false)
        XCTAssertNil(
            announcer.announce(
                knocks: [knock("192.0.2.24", id: "bbbbbbbbbbbbbbbb")], panelIsOpen: false),
            "a new instance id on the same address rang a second banner")
    }

    /// Two Macs arriving inside one read are ONE banner naming both, not two
    /// racing each other on screen.
    func testTwoArrivingInOneReadAreOneBanner() {
        var announcer = KnockAnnouncer()
        _ = announcer.announce(knocks: [], panelIsOpen: false)
        let notice = announcer.announce(
            knocks: [
                knock("192.0.2.24", name: "loft-mini"),
                knock("192.0.2.31", name: "attic-nuc", id: "3b1d90c47ae25f68"),
            ], panelIsOpen: false)
        XCTAssertEqual(notice?.title, "2 Macs want to connect")
        XCTAssertEqual(
            notice?.body,
            "192.0.2.24 and 192.0.2.31 · each expires in ten minutes. Nothing is shared "
                + "until you accept and both screens show the same six digits.")
        XCTAssertEqual(notice?.addresses, ["192.0.2.24", "192.0.2.31"])
    }

    /// A second Mac arriving later rings on its own, and the banner names only
    /// the Mac that is new: the first one was already announced.
    func testASecondMacRingsForItselfAlone() {
        var announcer = KnockAnnouncer()
        _ = announcer.announce(knocks: [], panelIsOpen: false)
        _ = announcer.announce(knocks: [knock("192.0.2.24", name: "loft-mini")], panelIsOpen: false)
        let second = announcer.announce(
            knocks: [
                knock("192.0.2.24", name: "loft-mini"),
                knock("192.0.2.31", name: "attic-nuc", id: "3b1d90c47ae25f68"),
            ], panelIsOpen: false)
        XCTAssertEqual(second?.title, "attic-nuc wants to connect")
        XCTAssertEqual(second?.addresses, ["192.0.2.31"])
    }

    /// Nothing while the panel is open: the card is on screen to be answered,
    /// and a banner over it is noise. The address is still recorded, because
    /// it has in fact been seen.
    func testNothingIsPostedWhileThePanelIsOpenAndItIsStillRecorded() {
        var announcer = KnockAnnouncer()
        _ = announcer.announce(knocks: [], panelIsOpen: false)
        XCTAssertNil(
            announcer.announce(
                knocks: [knock("192.0.2.24", name: "loft-mini")], panelIsOpen: true))
        XCTAssertNil(
            announcer.announce(
                knocks: [knock("192.0.2.24", name: "loft-mini")], panelIsOpen: false),
            "it rang the moment the panel closed, about a request already on screen")
    }

    /// **The rule that makes the whole thing re-armable.** An address that
    /// leaves the pending list is forgotten, so a Mac that was ignored,
    /// blocked or left to expire rings again when it asks a second time.
    func testAnAddressThatLeavesIsForgottenAndRingsAgain() {
        var announcer = KnockAnnouncer()
        _ = announcer.announce(knocks: [], panelIsOpen: false)
        _ = announcer.announce(knocks: [knock("192.0.2.24", name: "loft-mini")], panelIsOpen: false)
        XCTAssertNil(announcer.announce(knocks: [], panelIsOpen: false), "an empty read rang")
        XCTAssertFalse(announcer.hasAnnounced("192.0.2.24"))
        XCTAssertEqual(
            announcer.announce(
                knocks: [knock("192.0.2.24", name: "loft-mini")], panelIsOpen: false)?.title,
            "loft-mini wants to connect",
            "a Mac that asked again after being answered was silent forever")
    }

    /// Above three addresses the body names the first and counts the rest, the
    /// shape the tab's own held-back footer uses. A banner listing eight
    /// addresses is a banner nobody reads.
    func testAboveThreeAddressesTheBodyCountsTheRest() {
        var announcer = KnockAnnouncer()
        _ = announcer.announce(knocks: [], panelIsOpen: false)
        let many = (0..<4).map { i in
            knock("192.0.2.2\(i)", name: "mac-\(i)", id: "aaaaaaaaaaaaaaa\(i)")
        }
        let notice = announcer.announce(knocks: many, panelIsOpen: false)
        XCTAssertEqual(notice?.title, "4 Macs want to connect")
        XCTAssertEqual(
            notice?.body,
            "192.0.2.20 and 3 others · each expires in ten minutes. Nothing is shared until "
                + "you accept and both screens show the same six digits.")
    }

    /// A knock that proposed no name says so, on both lines, rather than
    /// drawing an empty title or repeating the address.
    func testAKnockWithNoNameIsAddressedByAddress() {
        var announcer = KnockAnnouncer()
        _ = announcer.announce(knocks: [], panelIsOpen: false)
        let notice = announcer.announce(knocks: [knock("192.0.2.31")], panelIsOpen: false)
        XCTAssertEqual(notice?.title, "192.0.2.31 wants to connect")
        XCTAssertEqual(
            notice?.body,
            "No name sent · expires in ten minutes. Nothing is shared until you accept and "
                + "both screens show the same six digits.")
    }

    /// The banner describes the deadline where every other surface counts it,
    /// and that is deliberate: it is written once and then sits in
    /// Notification Centre beside the time macOS stamps on it, so a counted
    /// `9m` would be a lie an hour later.
    func testTheBannerDescribesTheDeadlineRatherThanCountingIt() {
        let body = PeerAdmission.knockNoticeBody([knock("192.0.2.24", name: "loft-mini")])
        XCTAssertEqual(
            PeerAdmission.knockExpirySeconds, 600,
            "the figure the banner says in words moved, and the sentence did not")
        XCTAssertTrue(body?.contains("expires in ten minutes") ?? false, body ?? "nil")
        XCTAssertFalse(body?.contains("9m") ?? true, "the banner counted a deadline it cannot redraw")
    }

    /// Nothing to say about nobody.
    func testNoKnocksIsNoTitleAndNoBody() {
        XCTAssertNil(PeerAdmission.knockNoticeTitle([]))
        XCTAssertNil(PeerAdmission.knockNoticeBody([]))
    }
}
