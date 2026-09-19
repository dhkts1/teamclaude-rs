import XCTest

@testable import TcrBarCore

/// EVERY VERB THIS APP RUNS IS A VERB THE CLI HAS.
///
/// The Peers tab's live half shipped calling `tcr peer status --json` while the
/// CLI had no `status` under `peer` at all. Nothing caught it: `PeerCommand` is
/// an array of strings, the tab treats a non-zero exit as "this `tcr` is too
/// old to answer" and quietly keeps its file half, and both suites stayed green
/// while the live column was permanently blank. A wrong verb and an old binary
/// are indistinguishable at the call site, which is exactly why the call site
/// cannot be the thing that checks.
///
/// So this reads `src/main.rs`, the clap definition itself, rather than
/// restating a list of verbs that would drift the moment one is renamed. It is
/// the same shape as `FleetStatusTests.testAccountStatusErrorTokenStillExists
/// InRustSource`, and for the same reason: a rename fails THIS test loudly
/// instead of failing the panel silently in front of an operator.
///
/// What it does NOT prove: that the verb accepts these flags, or that its
/// output decodes. The first is clap's own business and the second is
/// `PeersStatusBlockTests`. This is the one link neither of those covers:
/// that the word exists at all.
final class PeerCommandVerbTests: XCTestCase {
    /// `src/main.rs`, read from the repository this test file lives in.
    private func rustMain() throws -> String {
        let thisFile = URL(fileURLWithPath: #filePath)
        let repoRoot =
            thisFile
            .deletingLastPathComponent()  // PeerCommandVerbTests.swift -> TcrBarTests
            .deletingLastPathComponent()  // TcrBarTests -> Tests
            .deletingLastPathComponent()  // Tests -> apps/macos
            .deletingLastPathComponent()  // apps/macos -> apps
            .deletingLastPathComponent()  // apps -> repo root
        return try String(
            contentsOf: repoRoot.appendingPathComponent("src/main.rs"), encoding: .utf8)
    }

    /// Every `tcr peer <verb>` this app can run, taken off `PeerCommand`
    /// itself, has a matching arm in the CLI's `PeerAction` enum.
    ///
    /// The arms are spelled `Status(PeerStatusArgs)` in Rust and `status` in
    /// argv, so the check is on the capitalized arm name, which is also why a
    /// verb spelled with a second word (there are none today) would need this
    /// test taught about it rather than passing by accident.
    func testEveryPeerVerbThisAppRunsExistsInTheCli() throws {
        let source = try rustMain()

        let invocations: [(label: String, argv: [String])] = [
            ("liveStatus", PeerCommand.liveStatus),
            ("list", PeerCommand.list),
            ("invite", PeerCommand.invite),
            ("regenerate", PeerCommand.regenerate),
            ("find(on:)", PeerCommand.find(on: true)),
            ("share(on:)", PeerCommand.share(on: true)),
            ("pair(address:)", PeerCommand.pair(address: "127.0.0.1:1")),
            ("forget(peer:)", PeerCommand.forget(peer: "p")),
            ("join(key:)", PeerCommand.join(key: "tcr-join:x").arguments),
        ]

        for invocation in invocations {
            XCTAssertEqual(
                invocation.argv.first, "peer",
                "\(invocation.label) must be a `tcr peer …` invocation")
            guard let verb = invocation.argv.dropFirst().first else {
                XCTFail("\(invocation.label) names no verb: \(invocation.argv)")
                continue
            }
            let arm = verb.prefix(1).uppercased() + verb.dropFirst()
            XCTAssertTrue(
                source.contains("\(arm)(Peer") || source.contains("\(arm)(peer_cli::"),
                "the panel runs `tcr peer \(verb)` (\(invocation.label)) and src/main.rs has no "
                    + "`\(arm)(…)` arm in `PeerAction`, the tab would read the non-zero exit as "
                    + "an older tcr and silently keep its file half, which is how "
                    + "`peer status` shipped as a verb nothing answered"
            )
        }
    }

    /// The live half is a DIFFERENT verb from the file half. Folding them
    /// together would make a proxy that is down look like a peers file that is
    /// empty, which is the one outcome the tab forbids by name.
    func testTheLiveHalfAndTheFileHalfAreNotTheSameVerb() {
        XCTAssertNotEqual(PeerCommand.liveStatus, PeerCommand.list)
        XCTAssertEqual(PeerCommand.liveStatus, ["peer", "status", "--json"])
        XCTAssertEqual(PeerCommand.list, ["peer", "ls", "--json"])
    }
}
