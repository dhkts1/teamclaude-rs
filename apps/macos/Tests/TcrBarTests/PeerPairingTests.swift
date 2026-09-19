import XCTest

@testable import TcrBarCore

/// The pairing a person can finish: the lines `tcr peer pair --json` prints,
/// the states they fold into, and the field the other Mac's digits are typed
/// into.
///
/// Every assertion here runs on values. Nothing in this file starts a process,
/// binds a port or reads the operator's config: `PeerPairRun` is the only part
/// that touches a subprocess and it is not built here.
final class PeerPairingTests: XCTestCase {

    // MARK: - The lines

    /// Each line `src/peer/pair.rs` writes decodes to the event it names.
    ///
    /// The literals are the ones `tests/peer_pairing.rs` pins on the other
    /// side, so the two halves of this wire are asserted against the same
    /// bytes rather than against each other's idea of them.
    func testEveryPrintedLineDecodes() {
        XCTAssertEqual(
            PeerPairEvent.decode(
                line:
                    #"{"event":"asking","addr":"10.0.1.24:7749","instance":"8f2c1ad63b0e4471","waitSeconds":600}"#
            ),
            .asking(addr: "10.0.1.24:7749", instance: "8f2c1ad63b0e4471", waitSeconds: 600))
        XCTAssertEqual(
            PeerPairEvent.decode(line: #"{"event":"comparing","code":"418902"}"#),
            .comparing(code: "418902"))
        XCTAssertEqual(
            PeerPairEvent.decode(line: #"{"event":"trusted","peer":"tcr-4b8we1r0zp"}"#),
            .trusted(peer: "tcr-4b8we1r0zp"))
        XCTAssertEqual(
            PeerPairEvent.decode(line: #"{"event":"refused","message":"peer pair: refused"}"#),
            .refused(message: "peer pair: refused"))
    }

    /// A key renamed on the Rust side decodes to nothing rather than to a
    /// half-filled event.
    ///
    /// This is the failure the whole `--json` contract exists to make loud:
    /// `waitSeconds` spelled `wait_seconds` would otherwise leave the sheet on
    /// its first state forever with every gate on both sides green.
    func testALineThisBuildCannotReadIsNotHalfDecoded() {
        XCTAssertNil(
            PeerPairEvent.decode(
                line: #"{"event":"asking","addr":"10.0.1.24:7749","instance":"8f","wait_seconds":600}"#
            ),
            "an asking line missing its wait decoded anyway, so the panel would arm a deadline "
                + "it invented")
        XCTAssertNil(PeerPairEvent.decode(line: #"{"event":"handshaking","code":"418902"}"#))
        XCTAssertNil(PeerPairEvent.decode(line: "peer pair: this Mac shows 418902"))
        XCTAssertNil(PeerPairEvent.decode(line: "   "))
    }

    // MARK: - The states

    /// The pairing's own order, folded one event at a time.
    func testTheStatesFollowTheCommand() {
        var state = PeerPairState.asking(instance: "")
        XCTAssertNil(
            state.farSideInstruction,
            "the sheet printed an instruction with a blank where the instance id belongs")
        state = state.applying(
            .asking(addr: "10.0.1.24:7749", instance: "8f2c1ad63b0e4471", waitSeconds: 600))
        XCTAssertEqual(state, .asking(instance: "8f2c1ad63b0e4471"))
        // The sentence on the sheet is what somebody at the other Mac can act
        // on from the panel they are already looking at. The two commands and
        // the instance id are still available, in the button's help, where a
        // bug report can find them and a person reading the sheet does not
        // have to.
        XCTAssertEqual(
            state.farSideInstruction,
            "On that Mac, the Peers tab shows this request and anybody there can press "
                + "Accept. Nothing has been disclosed to it yet.")
        XCTAssertFalse(
            state.farSideInstruction?.contains("`") ?? true,
            "the sheet prints backticks again, and they draw as the characters they are")
        XCTAssertEqual(
            state.farSideCommands,
            "On that Mac, tcr peer pending lists this request and tcr peer accept "
                + "8f2c1ad63b0e4471 approves it.")
        state = state.applying(.comparing(code: "418902"))
        XCTAssertEqual(state, .comparing(code: "418902"))
        XCTAssertTrue(state.isLive)
        state = state.applying(.trusted(peer: "tcr-4b8we1r0zp"))
        XCTAssertEqual(state, .done(peer: "tcr-4b8we1r0zp"))
        XCTAssertFalse(state.isLive)
    }

    /// A settled pairing is not moved by a late line.
    ///
    /// Both ends treat `trusted` and `refused` as terminal, so a sheet that
    /// walked off "pinned" would be this panel inventing a state the pairing
    /// does not have.
    func testAFinishedPairingIgnoresWhatComesAfter() {
        let done = PeerPairState.done(peer: "tcr-4b8we1r0zp")
        XCTAssertEqual(done.applying(.refused(message: "too late")), done)
        let cancelled = PeerPairState.cancelled
        XCTAssertEqual(cancelled.applying(.comparing(code: "418902")), cancelled)
    }

    /// The deadline comes off the wire, never from a number written in Swift.
    func testTheWaitIsTakenFromTheCommandsOwnFirstLine() {
        XCTAssertEqual(
            PeerPairState.wait(
                from: .asking(addr: "10.0.1.24:7749", instance: "8f", waitSeconds: 600)),
            600)
        XCTAssertNil(
            PeerPairState.wait(from: .comparing(code: "418902")),
            "a deadline was invented from an event that names no wait")
    }

    /// The words, per state. They are asserted because they are the whole
    /// surface: this sheet has no other output.
    func testTheWordsSayWhatIsActuallyTrue() {
        XCTAssertEqual(
            PeerPairState.asking(instance: "8f").title(peerName: "studio-mac"),
            "Waiting for studio-mac to accept",
            "the waiting sheet no longer shares the row's own fragment")
        let comparing = PeerPairState.comparing(code: "418902")
        XCTAssertEqual(comparing.title(peerName: "studio-mac"), "Compare the digits with studio-mac")
        let sentence = comparing.sentence(peerName: "studio-mac")
        XCTAssertTrue(
            sentence.contains("Read them off that screen and type them here"),
            "the compare sentence no longer tells the operator to read the OTHER screen, which "
                + "is the one act the six digits exist to force: \(sentence)")
        XCTAssertFalse(
            sentence.contains("the same six digits"),
            "the sheet asserts the two numbers match again; only the person looking at both "
                + "screens can know that: \(sentence)")
        XCTAssertEqual(
            PeerPairState.refused("peer pair: refused, 418902 here, 418903 there")
                .sentence(peerName: "studio-mac"),
            "peer pair: refused, 418902 here, 418903 there",
            "a refusal is paraphrased rather than given in the CLI's own words")
        for state in [
            PeerPairState.asking(instance: "8f"), .comparing(code: "418902"),
            .done(peer: "tcr-4b8we1r0zp"), .refused("no"), .cancelled,
        ] {
            XCTAssertFalse(
                state.sentence(peerName: "studio-mac").contains("\u{2014}"),
                "an em dash is in the pairing copy")
        }
    }

    // MARK: - The field

    /// The field keeps only what can be a code, and submits only a whole one.
    func testTheComparedFieldTakesSixDigitsAndNothingElse() {
        var compare = PeerPairCompare()
        compare.set("41")
        XCTAssertFalse(compare.isComplete)
        XCTAssertNil(compare.submission, "a half-typed code could be submitted")
        compare.set("418-902")
        XCTAssertEqual(
            compare.typed, "418902",
            "punctuation reached the CLI, which refuses it as \"not six digits\": a sentence "
                + "that reads as a mismatch and is not one")
        XCTAssertTrue(compare.isComplete)
        XCTAssertEqual(
            compare.submission, "418902\n",
            "the submission has no newline, so the CLI's read_line never returns and the "
                + "pairing hangs with the digits already typed")
        compare.set("4189021234")
        XCTAssertEqual(compare.typed, "418902", "more than six digits reached the field")
    }

    /// There is no "they match" control anywhere.
    ///
    /// A button spelled that way lets an operator confirm without ever looking
    /// at the other screen, which is the one thing this ritual exists to
    /// force. The refusal is recorded here rather than in prose alone.
    func testThereIsNoTheyMatchButton() throws {
        let tab = try String(
            contentsOf: repoRoot()
                .appendingPathComponent("apps/macos/Sources/TcrBar/PanelV4/PeersTabV4.swift"),
            encoding: .utf8)
        // Comment lines are dropped first, and that is not a loophole: the
        // sheet's own doc-comment RECORDS this refusal in as many words, so a
        // bare search over the file matches the sentence that forbids the
        // control and reports the control itself. What is searched is the code.
        let code = tab.split(separator: "\n", omittingEmptySubsequences: false)
            .filter { !$0.trimmingCharacters(in: .whitespaces).hasPrefix("//") }
            .joined(separator: "\n")
            .lowercased()
        XCTAssertTrue(
            code.contains("peertrustsheet"), "the search no longer reaches the sheet at all")
        XCTAssertFalse(
            code.contains("they match"),
            "a \"they match\" control is back: it confirms the compare without requiring it")
    }

    private func repoRoot() -> URL {
        URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent()
            .deletingLastPathComponent()
            .deletingLastPathComponent()
            .deletingLastPathComponent()
            .deletingLastPathComponent()
    }
}
