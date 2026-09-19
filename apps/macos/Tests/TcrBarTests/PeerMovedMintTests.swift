import XCTest

@testable import TcrBarCore

/// The minting run, classified.
///
/// The sheet behind the row's absent-path line draws one of three screens and
/// nothing else, so the classification is the whole of what it can get wrong:
/// a refusal drawn as a link box with nothing in it, a link drawn with two
/// sentences stuck to the end of it, or a run that never happened reported as
/// something `tcr` said.
final class PeerMovedMintTests: XCTestCase {

    /// A whole run, as the verb prints it: the link on its own line, then what
    /// it wants to say about it.
    private let printed = """
        tcr://peer/moved?v=1&r=6YFN2TQ8ZKW3H0JA5RC1XVB7MEPD9GNT4SZQ0W8KY2FH6JR3BXM5VCTQ81NPZ
        peer moved: sealed for studio-mac; only that Mac can read it, and it goes stale in 24 hours
        peer moved: it carries 2 address(es) and nothing else: it joins nothing, grants nothing and pairs nothing
        """

    /// The link is picked out of everything else the verb printed, because the
    /// box on screen is the thing a person copies into a chat window.
    func testTheLinkIsPickedOutOfWhatTheVerbPrintedAroundIt() {
        guard
            case .minted(let link, let sentences) = PeerMovedMint.outcome(
                exitCode: 0, stdout: printed, stderr: "")
        else { return XCTFail("a clean run that printed a link is not a minted link") }
        XCTAssertTrue(link.hasPrefix(PeerMovedMint.linkPrefix))
        XCTAssertFalse(
            link.contains("sealed for"),
            "the verb's own sentences are inside the string a person pastes into a chat")
        XCTAssertTrue(
            sentences.contains("goes stale in 24 hours"),
            "what tcr said about the link was dropped, and the sheet then states its own "
                + "version of how long a link is good for or states nothing at all")
        XCTAssertFalse(
            sentences.contains(PeerMovedMint.linkPrefix),
            "the link is printed twice, once in the box and once in the prose under it")
    }

    /// A note the verb prints BEFORE the link is still the verb's, and it is
    /// kept: it is the difference between a link a friend off this network can
    /// act on and one they cannot.
    func testALineAboveTheLinkIsKeptAsOneOfTheVerbsSentences() {
        let note =
            "peer moved: the router mapped a port and would not name its own external "
            + "address, so this link carries the listen socket alone"
        guard
            case .minted(let link, let sentences) = PeerMovedMint.outcome(
                exitCode: 0, stdout: "\(note)\n\(printed)", stderr: "")
        else { return XCTFail("a clean run with a note above the link is not a minted link") }
        XCTAssertTrue(link.hasPrefix(PeerMovedMint.linkPrefix))
        XCTAssertTrue(sentences.hasPrefix(note))
    }

    /// A refusal is `tcr`'s sentence, whatever it is, and it arrives on the
    /// stream the process chose. The pair with no shared secret is the one
    /// refusal a person can act on, and the remedy is inside the CLI's own
    /// words.
    func testARefusalIsCarriedInTheClisOwnWords() {
        let said =
            "moved link: this pair has no shared secret yet, so there is nothing only "
            + "the two of you can read; let the two Macs complete one session together and "
            + "try again"
        XCTAssertEqual(
            PeerMovedMint.outcome(exitCode: 1, stdout: "", stderr: said), .refused(said))
        XCTAssertEqual(
            PeerMovedMint.outcome(exitCode: 1, stdout: said, stderr: ""), .refused(said),
            "a refusal the process printed on stdout is reported as a run that never "
                + "happened, so the one sentence with the remedy in it is thrown away")
    }

    /// And a run that said nothing at all is this app's own words, never
    /// dressed up as something `tcr` refused: the two have different fixes.
    func testARunThatSaidNothingIsNotARefusal() {
        guard
            case .couldNotRun(let sentence) = PeerMovedMint.outcome(
                exitCode: 2, stdout: "  \n", stderr: "")
        else { return XCTFail("a silent failure is reported as a refusal tcr made") }
        XCTAssertTrue(sentence.contains("exit 2"))
    }

    /// A clean exit with no link on stdout is not a link. An empty box with a
    /// Copy button under it is the one screen this sheet may not draw.
    func testACleanRunWithNoLinkIsNotAMintedLink() {
        guard
            case .couldNotRun = PeerMovedMint.outcome(
                exitCode: 0, stdout: "peer moved: nothing to say\n", stderr: "")
        else { return XCTFail("a run that printed no link is drawn as a link") }
    }

    /// What a link looks like is ``PeerMovedLink``'s answer, not a second copy
    /// of the scheme spelled out here.
    func testTheLinkPrefixComesFromTheLinkTypeItself() {
        XCTAssertEqual(
            PeerMovedMint.linkPrefix,
            "\(PeerMovedLink.scheme)://\(PeerMovedLink.host)\(PeerMovedLink.path)")
        XCTAssertEqual(
            PeerMovedLink.route(URL(fileURLWithPath: "/x")), .neither,
            "a positive control on the other half: the router still answers about a URL that "
                + "is not one of ours")
    }
}
