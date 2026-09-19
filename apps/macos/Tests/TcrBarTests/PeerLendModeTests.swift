import XCTest

@testable import TcrBarCore

/// The grant's mode, its argv, and the winding-down state a two-option
/// control cannot show on its own.
final class PeerLendModeTests: XCTestCase {
    private let now = Date(timeIntervalSince1970: 1_786_000_000)

    private func grant(
        mode: LendMode, handedKeyUntil: Int64? = nil
    ) -> PeerLendGrant {
        PeerLendGrant(
            leaseId: "ls-4b1f", scope: .group("work"), window: .week, fraction: 0.20,
            mode: mode, handedKeyUntil: handedKeyUntil)
    }

    // MARK: The wire

    /// A grant written before modes existed carries no `mode` key at all
    /// (`LendMode::is_serve` is the producer's `skip_serializing_if`), and it
    /// already meant serve. Decoding absent as anything else would report a
    /// handed key on every lease this mesh has ever written.
    func testAbsentModeDecodesAsServe() throws {
        let json = """
            {"id":"ls-1","scope":"all","window":"7d","fraction":0.2}
            """
        let decoded = try JSONDecoder().decode(PeerLendGrant.self, from: Data(json.utf8))
        XCTAssertEqual(decoded.mode, .serve)
        XCTAssertNil(decoded.handedKeyUntil)
        // The producer's own wire key is `id`, never `leaseId`
        // (`LendGrant::id`, `#[serde(with = "lease_id_hex")]`): decoding the
        // wrong key read every real grant's id as `""`, so `--revoke` and
        // `--relend` ran against no lease at all.
        XCTAssertEqual(decoded.leaseId, "ls-1")
    }

    func testHandModeAndItsKeyExpiryDecode() throws {
        let json = """
            {"id":"ls-1","scope":"all","window":"7d","fraction":0.2,
             "mode":"hand","handedKeyUntil":1786000200}
            """
        let decoded = try JSONDecoder().decode(PeerLendGrant.self, from: Data(json.utf8))
        XCTAssertEqual(decoded.mode, .hand)
        XCTAssertEqual(decoded.handedKeyUntil, 1_786_000_200)
    }

    /// A mode this build cannot name is serve, the shipped default, rather
    /// than a decode that throws away a live lease's numbers.
    func testAnUnknownModeTokenIsServe() {
        XCTAssertEqual(LendMode(token: "borrowed-somehow"), .serve)
        XCTAssertEqual(LendMode(token: nil), .serve)
        XCTAssertEqual(LendMode(token: "hand"), .hand)
    }

    // MARK: Argv

    /// One Save sends the whole lease, and the handed mode adds one flag to
    /// it. A serve lease writes NO `--mode`, which is the producer's own
    /// asymmetry (`skip_serializing_if`): every lease already written means
    /// serve without the word, so an ordinary write is unchanged and the new
    /// flag reaches `tcr` only when somebody asked for the handed mode.
    func testLendArgvCarriesTheModeOnlyWhenItIsHanded() {
        let serve = PeerCommand.lend(
            peer: "tcr-4b8we1r0zp", scope: .all, terms: .standard(for: .week), mode: .serve)
        let hand = PeerCommand.lend(
            peer: "tcr-4b8we1r0zp", scope: .all, terms: .standard(for: .week), mode: .hand)
        XCTAssertFalse(serve.contains("--mode"))
        XCTAssertEqual(Array(hand[(hand.count - 2)...]), ["--mode", "hand"])
        // The handed argv is the serve argv plus those two words, in order.
        XCTAssertEqual(Array(hand.dropLast(2)), serve)
    }

    /// A draft opened over an existing lease keeps that lease's mode, so
    /// pressing Save without touching the control cannot silently move a
    /// handed lease back to serve.
    func testEditingADraftKeepsTheGrantsMode() {
        let draft = LeaseDraft(editing: grant(mode: .hand), peer: "tcr-4b8we1r0zp")
        XCTAssertEqual(draft.mode, .hand)
        let argv = draft.arguments
        XCTAssertEqual(Array(argv[(argv.count - 2)...]), ["--mode", "hand"])
    }

    func testANewLeaseSendsServe() {
        XCTAssertEqual(LeaseDraft.new(peer: "tcr-4b8we1r0zp").mode, .serve)
    }

    // MARK: The three states

    /// Serve, nothing handed: no status line at all. A line here would be an
    /// invented fact about a key that does not exist.
    func testServeWithNoHandedKeyHasNoStatusLine() {
        XCTAssertNil(PeerLease.handedKeyLine(grant(mode: .serve), peer: "studio-mac", now: now))
    }

    func testHandWithALiveKeyReadsAsRenewing() {
        let line = PeerLease.handedKeyLine(
            grant(mode: .hand, handedKeyUntil: Int64(now.timeIntervalSince1970) + 262),
            peer: "studio-mac", now: now)
        XCTAssertEqual(line?.winding, false)
        XCTAssertEqual(line?.text.hasPrefix("Renewing normally"), true)
    }

    /// THE state scene 2c exists for: the operator pressed "Over this Mac" and
    /// the borrower's old key is still good. Amber, and it names the remaining
    /// time rather than implying the switch took effect at once.
    func testSwitchedBackToServeWithAKeyStillLiveIsTheWindingDownState() {
        let line = PeerLease.handedKeyLine(
            grant(mode: .serve, handedKeyUntil: Int64(now.timeIntervalSince1970) + 166),
            peer: "studio-mac", now: now)
        XCTAssertEqual(line?.winding, true)
        XCTAssertEqual(
            line?.text, "studio-mac's old key is not renewing and expires on its own in 2m")
    }

    /// Once that key has expired there is nothing left to warn about, so the
    /// line goes away rather than counting into the negative.
    func testAnExpiredOldKeyDrawsNoLine() {
        XCTAssertNil(
            PeerLease.handedKeyLine(
                grant(mode: .serve, handedKeyUntil: Int64(now.timeIntervalSince1970) - 1),
                peer: "studio-mac", now: now))
    }

    /// A hand grant whose producer reports no expiry says the mode is on and
    /// claims no countdown: absent is an absence, never a guessed clock.
    func testHandWithNoReportedExpirySaysOnlyWhatIsKnown() {
        let line = PeerLease.handedKeyLine(grant(mode: .hand), peer: "studio-mac", now: now)
        XCTAssertEqual(line, PeerLease.HandedKeyLine(text: "Renewing normally", winding: false))
    }

    // MARK: Words

    func testTheTwoSentencesDifferAndCarryNoWireVocabulary() {
        let serve = PeerLease.modeSentence(.serve, peer: "studio-mac")
        let hand = PeerLease.modeSentence(.hand, peer: "studio-mac")
        XCTAssertNotEqual(serve, hand)
        for sentence in [serve, hand, LendMode.serve.label, LendMode.hand.label] {
            for word in ["IK", "NAT-PMP", "UPnP", "egress", "serve mode", "hand mode"] {
                XCTAssertFalse(
                    sentence.contains(word), "\(word) reached a user-visible string")
            }
        }
        XCTAssertEqual(LendMode.serve.label, "Over this Mac")
        XCTAssertEqual(LendMode.hand.label, "With a handed key")
    }
}
