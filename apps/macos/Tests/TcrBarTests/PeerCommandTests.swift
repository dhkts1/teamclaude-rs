import XCTest

@testable import TcrBarCore

/// Every `tcr peer …` this app runs, as values rather than as source text.
///
/// `PeerCommand` used to live in the `TcrBar`
/// executable target, so `PeersPanelWiringTests` could only assert that a
/// LINE OF SOURCE containing the right argv existed somewhere in a 1700-line
/// view; those assertions are here now, against the argv itself.
final class PeerCommandTests: XCTestCase {

    /// A key shaped like the real one (`pair.rs`'s `tcr-join:v1:…`), long
    /// enough that a substring match against argv means something.
    private let key = "tcr-join:v1:C3H6MZVQ7K2XB4YFD5NRTA8WEJ"

    // MARK: - The join key never enters argv (item 1)

    /// The whole of item 1. `ps` shows argv to every process on this Mac and
    /// the key is the credential that turns an unknown Mac into a trusted one,
    /// so it goes down a pipe: `tcr peer join --stdin`, key on stdin.
    func testTheJoinKeyIsOnStdinAndNotInArgv() {
        let invocation = PeerCommand.join(key: key)

        XCTAssertEqual(invocation.arguments, ["peer", "join", "--stdin"])
        XCTAssertEqual(invocation.stdin, key)
        XCTAssertFalse(
            invocation.secretIsInArgv,
            "the join key is in argv, where `ps` shows it to every process on this Mac")
    }

    /// The leak this type exists to catch, caught. A `PeerSecretInvocation`
    /// built the old way (the key as an argument) answers `true`, which is
    /// what makes the assertion above a gate rather than a restatement of
    /// whatever `join(key:)` happens to return.
    func testTheArgvCheckCatchesAKeyPassedAsAnArgument() {
        XCTAssertTrue(
            PeerSecretInvocation(arguments: ["peer", "join", key], stdin: key).secretIsInArgv)
        XCTAssertTrue(
            PeerSecretInvocation(arguments: ["peer", "join", "--key=\(key)"], stdin: key)
                .secretIsInArgv,
            "a key glued to a flag is the same leak, spelled differently")
    }

    /// An empty secret is not a leak in every argument. `contains("")` is true
    /// of every string, so the naive check would call a nil-secret invocation
    /// leaked and make the real assertion above useless.
    func testAnEmptySecretIsNotFoundInEveryArgument() {
        XCTAssertFalse(
            PeerSecretInvocation(arguments: ["peer", "join", "--stdin"], stdin: "")
                .secretIsInArgv)
    }

    /// What the pane may show and log: the argv, never the bytes.
    func testTheDisplayCommandCarriesNoSecret() {
        let shown = PeerCommand.join(key: key).displayCommand
        XCTAssertEqual(shown, "peer join --stdin")
        XCTAssertFalse(shown.contains(key))
    }

    // MARK: - The rest of the verbs

    /// `--yes` is what makes the pane's `confirmationDialog` the one and only
    /// confirmation. Without it the CLI asks again, on a terminal nobody is
    /// looking at, and the button appears to have done nothing.
    func testRegeneratePassesYesSoTheCliDoesNotAskAgain() {
        XCTAssertEqual(PeerCommand.regenerate, ["peer", "id", "--regenerate", "--yes"])
    }

    /// The mockup's own flags (`settings-peers.html:318`): one use, ten
    /// minutes. `--ttl` is `PeerInviteArgs::ttl`, a `u32` count of SECONDS
    /// (`src/main.rs`, `default_value_t = 600`), never a duration string:
    /// `"10m"` refused every invite with clap's own "invalid digit found in
    /// string" and the pane never had a join key to show.
    func testTheInviteIsOneUseAndTenMinutes() {
        XCTAssertEqual(PeerCommand.invite, ["peer", "invite", "--ttl", "600", "--uses", "1"])
    }

    /// `PeerNetworkKeyAction::Show`'s own argv (`src/main.rs`): says whether a
    /// key is set, never the key.
    func testNetworkKeyShowIsTheShowVerb() {
        XCTAssertEqual(PeerCommand.networkKeyShow, ["peer", "network-key", "show"])
    }

    /// A switch's argv is the state it moves TO, never the state it is in
    /// (the mockup's rule 4). Reversed, both switches would report the truth
    /// and change nothing.
    func testEachSwitchRunsTheStateItMovesTo() {
        XCTAssertEqual(PeerCommand.find(on: true), ["peer", "find", "on"])
        XCTAssertEqual(PeerCommand.find(on: false), ["peer", "find", "off"])
        XCTAssertEqual(PeerCommand.share(on: true), ["peer", "share", "on"])
        XCTAssertEqual(PeerCommand.share(on: false), ["peer", "share", "off"])
    }

    func testTheVerbsThatNameOneMacCarryIt() {
        XCTAssertEqual(PeerCommand.pair(address: "10.0.0.4:7749"), ["peer", "pair", "10.0.0.4:7749"])
        XCTAssertEqual(PeerCommand.forget(peer: "studio-mac"), ["peer", "forget", "studio-mac"])
    }

    /// The tab reads rows as JSON, never as the human table.
    func testTheListIsReadAsJson() {
        XCTAssertEqual(PeerCommand.list, ["peer", "ls", "--json"])
    }
}
