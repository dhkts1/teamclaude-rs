import XCTest

@testable import TcrBarCore

/// The minting run, classified.
///
/// The sheet behind the footer's `Invite…` button draws one of three screens
/// and nothing else, so the classification is the whole of what it can get
/// wrong: a truncated key in the box a person copies from, a refusal drawn as
/// an empty box, or a run that never happened reported as something `tcr`
/// said.
final class PeerInviteMintTests: XCTestCase {

    /// A whole run, as the verb prints it: the `ok` line, the key on its own
    /// line, one line per address in dial order, then the join-capable
    /// sentence. Addresses are documentation ranges, never a real one.
    private let printed = """
        peer invite: ok id=7 label=joining-mac ttl_s=600 uses=1
        tcr-join:v2:192.0.2.10:7755,198.51.100.20:7755:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA:BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB
        peer invite: lan 192.0.2.10:7755
        peer invite: tailscale 198.51.100.20:7755
        peer invite: this key is join-capable by anything that can read peers.json until it is used or expires, `tcr peer invite --revoke 7` ends it early
        """

    /// The key line is picked out and every other line is kept, in order.
    func testTheKeyLineIsPickedOutAndEveryOtherLineIsKeptInOrder() {
        guard
            case .minted(let key, let sentences) = PeerInviteMint.outcome(
                exitCode: 0, stdout: printed, stderr: "")
        else { return XCTFail("a clean run that printed a key is not a minted key") }
        XCTAssertTrue(key.hasPrefix(PeerInviteMint.keyPrefix))
        XCTAssertFalse(
            key.contains("peer invite:"),
            "the verb's own sentences are inside the string a person pastes into a chat")
        let lines = sentences.split(separator: "\n", omittingEmptySubsequences: false).map(String.init)
        XCTAssertEqual(
            lines,
            [
                "peer invite: ok id=7 label=joining-mac ttl_s=600 uses=1",
                "peer invite: lan 192.0.2.10:7755",
                "peer invite: tailscale 198.51.100.20:7755",
                "peer invite: this key is join-capable by anything that can read peers.json "
                    + "until it is used or expires, `tcr peer invite --revoke 7` ends it early",
            ],
            "the non-key lines survived the run in a different order than tcr printed them")
    }

    /// Positive control: the same fixture with the token line removed is
    /// `.couldNotRun`, never `.minted` with an empty key.
    func testTheSameFixtureWithTheTokenLineRemovedIsNotMinted() {
        let withoutKey = printed.split(separator: "\n", omittingEmptySubsequences: false)
            .filter { !$0.hasPrefix(PeerInviteMint.keyPrefix) }
            .joined(separator: "\n")
        guard
            case .couldNotRun = PeerInviteMint.outcome(exitCode: 0, stdout: withoutKey, stderr: "")
        else {
            return XCTFail(
                "a run with no key line on stdout is minted, which means a key box can be "
                    + "drawn empty")
        }
    }

    /// A refusal is `tcr`'s sentence, whatever it is: no listener, no address
    /// to reach, or the eight-outstanding cap, all carried in the CLI's own
    /// words.
    func testARefusalIsCarriedInTheClisOwnWords() {
        let said =
            "peer invite: eight invites are already outstanding (MAX_OUTSTANDING_INVITES); "
            + "every one of them is a PSK the registrar must try, so revoke one with "
            + "`tcr peer invite --revoke <id>` before minting another"
        XCTAssertEqual(
            PeerInviteMint.outcome(exitCode: 1, stdout: "", stderr: said), .refused(said))
        XCTAssertEqual(
            PeerInviteMint.outcome(exitCode: 1, stdout: said, stderr: ""), .refused(said),
            "a refusal the process printed on stdout is reported as a run that never "
                + "happened, so the one sentence with the remedy in it is thrown away")
    }

    /// A non-zero exit with silence is this app's own words, never dressed up
    /// as something `tcr` refused: the two have different fixes.
    func testANonZeroExitWithSilenceIsNotARefusal() {
        guard
            case .couldNotRun(let sentence) = PeerInviteMint.outcome(
                exitCode: 2, stdout: "  \n", stderr: "")
        else { return XCTFail("a silent failure is reported as a refusal tcr made") }
        XCTAssertTrue(sentence.contains("exit 2"))
    }

    /// A clean exit with no key on stdout is not a key. An empty box with a
    /// Copy button under it is the one screen this sheet may not draw.
    func testACleanRunWithNoKeyIsNotAMintedKey() {
        guard
            case .couldNotRun = PeerInviteMint.outcome(
                exitCode: 0, stdout: "peer invite: nothing to say\n", stderr: "")
        else { return XCTFail("a run that printed no key is drawn as a key") }
    }
}
