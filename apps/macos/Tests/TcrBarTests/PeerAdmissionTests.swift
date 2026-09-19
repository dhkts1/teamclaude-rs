import XCTest

@testable import TcrBarCore

/// Decision row 10's pairing request and row 11's caps, as the rows the Peers
/// tab and Settings > Advanced draw.
final class PeerAdmissionTests: XCTestCase {

    // MARK: - The pending row

    /// Scene 59's title, verbatim. The proposed name AND the address, because
    /// the name is a string a stranger on this network chose and the address
    /// is the part an operator can check.
    func testTheKnockTitleCarriesBothTheNameAndTheAddress() {
        let knock = PeerKnock(
            addr: "10.0.1.24", instanceId: "8f2c1ad63b0e4471", proposedName: "loft-mini")
        XCTAssertEqual(PeerAdmission.knockTitle(knock), "loft-mini (10.0.1.24) wants to connect")
    }

    func testAKnockWithNoNameShowsItsAddressAlone() {
        let knock = PeerKnock(addr: "10.0.1.24", instanceId: "8f2c1ad63b0e4471")
        XCTAssertEqual(PeerAdmission.knockTitle(knock), "10.0.1.24 wants to connect")
        XCTAssertEqual(
            PeerAdmission.knockTitle(
                PeerKnock(addr: "10.0.1.24", instanceId: "aa", proposedName: "")),
            "10.0.1.24 wants to connect",
            "an empty name is the same state as no name and must not draw empty brackets")
    }

    /// The deadline is COUNTED, from the knock's own first seen time.
    ///
    /// The card stated it in prose, "a request nobody answers expires by
    /// itself in ten minutes", and never counted it, while the knock carries
    /// the timestamp the count needs.
    func testTheKnockCountsItsOwnDeadline() {
        let now = Date(timeIntervalSince1970: 1_700_000_000)
        let knock = PeerKnock(
            addr: "10.0.1.24", instanceId: "8f2c1ad63b0e4471", proposedName: "loft-mini",
            firstSeenMs: Int64((now.timeIntervalSince1970 - 180) * 1000))
        XCTAssertEqual(PeerAdmission.knockExpiry(firstSeenMs: knock.firstSeenMs, now: now), "7m")
        XCTAssertEqual(
            PeerAdmission.knockAddressLine(knock, now: now), "10.0.1.24 · expires in 7m",
            "the address line no longer counts the deadline the knock's own timestamp buys")

        // A knock older than the deadline, and one whose producer sent no
        // timestamp at all, both count NOTHING rather than a negative span or
        // an invented one.
        let stale = PeerKnock(
            addr: "10.0.1.24", instanceId: "8f", proposedName: "loft-mini",
            firstSeenMs: Int64((now.timeIntervalSince1970 - 3600) * 1000))
        XCTAssertNil(PeerAdmission.knockExpiry(firstSeenMs: stale.firstSeenMs, now: now))
        XCTAssertEqual(PeerAdmission.knockAddressLine(stale, now: now), "10.0.1.24")
        XCTAssertNil(
            PeerAdmission.knockExpiry(firstSeenMs: 0, now: now),
            "a knock with no timestamp got a deadline out of the epoch")

        // With no name proposed the name line IS the address, so the second
        // line is the count alone rather than the address twice.
        let unnamed = PeerKnock(
            addr: "10.0.1.24", instanceId: "8f",
            firstSeenMs: Int64((now.timeIntervalSince1970 - 180) * 1000))
        XCTAssertEqual(PeerAdmission.knockAddressLine(unnamed, now: now), "expires in 7m")
    }

    /// Every verb on the row takes the INSTANCE ID, not the name: two knocks
    /// can propose one name, and `src/main.rs:201-207` takes the instance id
    /// or the address.
    func testAcceptIgnoreAndBlockTakeTheInstanceId() {
        XCTAssertEqual(
            PeerCommand.accept(instance: "8f2c1ad6"), ["peer", "accept", "8f2c1ad6"])
        XCTAssertEqual(
            PeerCommand.ignore(instance: "8f2c1ad6"), ["peer", "ignore", "8f2c1ad6"])
        XCTAssertEqual(PeerCommand.block(instance: "8f2c1ad6"), ["peer", "block", "8f2c1ad6"])
    }

    /// And Unblock takes the ADDRESS, a different argument from the other
    /// three, because a block outlives the boot the instance id belonged to.
    /// `src/main.rs:210-217` says so: "the address to unblock, as `tcr peer ls
    /// --json` lists it under `blocked`".
    func testUnblockTakesTheAddressAndNotTheInstanceId() {
        XCTAssertEqual(
            PeerCommand.unblock(address: "10.0.1.99"), ["peer", "unblock", "10.0.1.99"])
    }

    // MARK: - The limited footer

    func testTheLimitedFooterNamesBothNumbers() {
        XCTAssertEqual(PeerAdmission.limitedFooter(shown: 12, limited: 3), "12 shown, 3 more not shown")
        XCTAssertEqual(PeerAdmission.limitedFooter(shown: 12, limited: 1), "12 shown, 1 more not shown")
    }

    /// Nothing held back, no footer. A permanent "0 more not shown" is noise
    /// on every Mac that has never hit a cap.
    func testTheFooterIsAbsentWhenNothingWasHeldBack() {
        XCTAssertNil(PeerAdmission.limitedFooter(shown: 2, limited: 0))
    }

    // MARK: - Muted and blocked

    func testAMuteSaysWhenItLifts() {
        let now = Date(timeIntervalSince1970: 1_700_000_000)
        XCTAssertEqual(
            PeerAdmission.muteSentence(
                PeerMute(addr: "10.0.1.55", untilMs: 1_700_002_820_000), now: now),
            "quiet for another 47 min")
        XCTAssertEqual(
            PeerAdmission.muteSentence(
                PeerMute(addr: "10.0.1.55", untilMs: 1_700_000_030_000), now: now),
            "quiet for another 30 s")
    }

    func testAMuteWhoseHourIsUpSaysThat() {
        let now = Date(timeIntervalSince1970: 1_700_000_000)
        XCTAssertEqual(
            PeerAdmission.muteSentence(
                PeerMute(addr: "10.0.1.55", untilMs: 1_699_999_000_000), now: now),
            "the hour is up; it may knock again")
    }

    /// The reason in the operator's words, and the key when one was learned,
    /// which is the field that makes the block survive DHCP handing that Mac a
    /// new address.
    func testABlockSaysWhyAndWhetherItCoversTheKey() {
        let now = Date(timeIntervalSince1970: 1_700_000_000)
        let byAddress = PeerBan(addr: "10.0.1.99", sinceMs: 1_699_989_200_000, reason: .blocked)
        XCTAssertEqual(
            PeerAdmission.blockSentence(byAddress, now: now),
            "blocked 3h ago · you pressed Block on its request to connect")

        let withKey = PeerBan(
            addr: "10.0.1.99", key: "NNNN", sinceMs: 1_699_989_200_000,
            reason: .forgottenAndBlocked)
        let sentence = PeerAdmission.blockSentence(withKey, now: now)
        XCTAssertTrue(sentence.contains("stopped trusting it and blocked it in one act"))
        XCTAssertTrue(
            sentence.contains("and its key too"),
            "a ban that covers the key has to say so: that is what keeps it banned from a "
                + "new address")
    }

    // MARK: - Caps

    /// The Advanced pane prints what the BINARY sent. These are the values
    /// `src/main.rs:1202-1208` reads out of `peer::{discovery,listener,state}`,
    /// and the pane no longer carries a second copy of any of them.
    func testTheAdvancedRowsAreDerivedFromTheCapsObject() {
        let caps = PeerCaps(
            foundRows: 12, foundPerAddress: 2, pending: 8, knockIntervalMs: 10000,
            knockBurst: 3, unauthenticatedSockets: 16)
        XCTAssertEqual(
            caps.advancedRows.map(\.value), ["12", "2", "8", "1 every 10 s, burst 3", "16"])
        XCTAssertEqual(caps.advancedRows.map(\.label).first, "Macs shown at once")
    }

    /// A cap that changes in the Rust changes on the pane, with no edit here.
    /// This is the property the hard-coded numbers could not have.
    func testChangedCapsChangeTheRowsWithNoEditToThePanel() {
        let caps = PeerCaps(
            foundRows: 20, foundPerAddress: 4, pending: 16, knockIntervalMs: 2500,
            knockBurst: 5, unauthenticatedSockets: 32)
        XCTAssertEqual(
            caps.advancedRows.map(\.value), ["20", "4", "16", "1 every 2.5 s, burst 5", "32"])
    }

    /// Sub-second intervals stay in milliseconds rather than rounding to
    /// `0 s`, which would print a cap of "one knock every no time at all".
    func testASubSecondIntervalKeepsItsMilliseconds() {
        let caps = PeerCaps(
            foundRows: 1, foundPerAddress: 1, pending: 1, knockIntervalMs: 250, knockBurst: 1,
            unauthenticatedSockets: 1)
        XCTAssertEqual(caps.knockRate, "1 every 250 ms, burst 1")
    }
}
