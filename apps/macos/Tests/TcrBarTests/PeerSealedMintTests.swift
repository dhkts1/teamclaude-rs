import XCTest

@testable import TcrBarCore

/// The sealed exchange's three runs, classified.
///
/// The invite sheet's second segment and the paste sheet's ask branch both
/// draw one of five screens off this classifier, so what it gets wrong is
/// what a person sees: an ask or a reply truncated in the box they copy
/// from, a refusal drawn as an empty box, or a run that never happened
/// reported as something `tcr` said. ``PeerInviteMintTests``' own shape.
final class PeerSealedMintTests: XCTestCase {

    /// `tcr peer invite --sealed`'s whole run: the ask, then the two
    /// sentences about what it grants and how long it lasts.
    private let askRun = """
        tcr-invite:v1:71HFE865V215DE1CTEWH0AVNH9DN65R6PPG2X7R5JQC42EJ5E6RHXQAQPDH4
        peer invite: this names no address and grants nothing; whoever holds it can send you an address and nothing else
        peer invite: it is good for ten minutes; paste what comes back with `tcr peer invite --reply --stdin`
        """

    /// `tcr peer join --stdin`'s whole run, fed an ask: the reply, then the
    /// two sentences about sending it back.
    private let replyRun = """
        tcr-reply:v1:M9S346Q3D25VT4F5V37E3S3E28JT97KB6CQ643DZVMXXQKFBF5KZNWJ47TAN9ZT
        peer join: send this back to whoever sent you the invite; nothing in it says where you are except to them
        peer join: opening it joins you immediately, pinned and trusted on both sides, the same as a pasted key
        """

    /// The ask line is picked out, and every other line is kept in order.
    func testTheAskLineIsPickedOut() {
        guard
            case .asked(let ask, let sentences) = PeerSealedMint.outcome(
                exitCode: 0, stdout: askRun, stderr: "")
        else { return XCTFail("a clean run that printed an ask is not asked") }
        XCTAssertTrue(ask.hasPrefix(PeerSealedMint.askPrefix))
        XCTAssertFalse(
            ask.contains("peer invite:"),
            "the verb's own sentences are inside the string a person pastes into a chat")
        XCTAssertEqual(
            sentences,
            "peer invite: this names no address and grants nothing; whoever holds it can "
                + "send you an address and nothing else\n"
                + "peer invite: it is good for ten minutes; paste what comes back with "
                + "`tcr peer invite --reply --stdin`")
    }

    /// The reply line is picked out, ahead of the ask prefix, and every
    /// other line is kept in order.
    func testTheReplyLineIsPickedOut() {
        guard
            case .answered(let reply, let sentences) = PeerSealedMint.outcome(
                exitCode: 0, stdout: replyRun, stderr: "")
        else { return XCTFail("a clean run that printed a reply is not answered") }
        XCTAssertTrue(reply.hasPrefix(PeerSealedMint.replyPrefix))
        XCTAssertFalse(reply.contains("peer join:"))
        XCTAssertEqual(
            sentences,
            "peer join: send this back to whoever sent you the invite; nothing in it says "
                + "where you are except to them\n"
                + "peer join: opening it joins you immediately, pinned and trusted on both "
                + "sides, the same as a pasted key")
    }

    /// Opening a reply prints no blob at all: the join it carried ran
    /// immediately, and what is left is `tcr`'s own sentences.
    func testACleanRunWithNoBlobIsJoinedFromItsSentences() {
        let printed = "peer invite: ok addr=198.51.100.20:7755 file=tcr-peers.json"
        guard
            case .joined(let sentences) = PeerSealedMint.outcome(
                exitCode: 0, stdout: printed, stderr: "")
        else { return XCTFail("an ok line with no ask or reply prefix is not joined") }
        XCTAssertEqual(sentences, printed)
    }

    /// Exit 0 with nothing printed at all, blob or sentence, is never a
    /// blob that happens to be empty.
    func testExitZeroWithNoBlobAndNoSentenceIsCouldNotRun() {
        guard
            case .couldNotRun = PeerSealedMint.outcome(exitCode: 0, stdout: "  \n", stderr: "")
        else { return XCTFail("a run with nothing on stdout is drawn as a blob or a sentence") }
    }

    /// A refusal is `tcr`'s sentence, whatever it is: a reply that did not
    /// open, one already spent, or nothing answering at the address it
    /// carried.
    func testARefusalIsCarriedInTheClisOwnWords() {
        let said =
            "Error: peer join: this reply did not open against anything outstanding here; "
            + "it may answer an ask that already expired, was already spent, or belongs to "
            + "a different Mac"
        XCTAssertEqual(
            PeerSealedMint.outcome(exitCode: 1, stdout: "", stderr: said), .refused(said))
        XCTAssertEqual(
            PeerSealedMint.outcome(exitCode: 1, stdout: said, stderr: ""), .refused(said),
            "a refusal the process printed on stdout is reported as a run that never "
                + "happened, so the one sentence with the remedy in it is thrown away")
    }

    /// A non-zero exit with silence is this app's own words, never dressed
    /// up as something `tcr` refused.
    func testANonZeroExitWithSilenceIsNotARefusal() {
        guard
            case .couldNotRun(let sentence) = PeerSealedMint.outcome(
                exitCode: 2, stdout: "  \n", stderr: "")
        else { return XCTFail("a silent failure is reported as a refusal tcr made") }
        XCTAssertTrue(sentence.contains("exited 2"))
    }
}
