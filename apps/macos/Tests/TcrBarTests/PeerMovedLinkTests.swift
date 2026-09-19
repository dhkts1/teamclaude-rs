import XCTest

@testable import TcrBarCore

/// The `tcr://peer/moved` link: which handler it reaches, what the app may run
/// when one arrives, and what it may say when it refuses one.
///
/// Every value in these fixtures is obviously fake: the sealed records are
/// `AAAA…`/`BBBB…` and every address is from the documentation range
/// (192.0.2.0/24, 198.51.100.0/24). A real address or a real Mac's name in a
/// test file would be a disclosure in a public repository.
final class PeerMovedLinkTests: XCTestCase {

    private func url(_ string: String) throws -> URL {
        try XCTUnwrap(URL(string: string))
    }

    // MARK: Which handler a link reaches

    /// **A moved link is never handed to the join handler, and a join link is
    /// never handed to the moved one.**
    ///
    /// The two share a scheme, and the join handler was the only thing behind
    /// it. A second path that fell through to it would pipe a moved link into
    /// `tcr peer join --stdin`, the verb that sets this Mac's network key.
    func testAMovedLinkIsNeverHandedToTheJoinHandler() throws {
        let moved = try url("tcr://peer/moved?v=1&r=AAAABBBBCCCCDDDD")
        XCTAssertEqual(PeerMovedLink.route(moved), .moved)
        switch PeerJoinLink.invocation(for: moved) {
        case .success(let invocation):
            XCTFail(
                "a moved link was accepted as a join and would have run "
                    + invocation.displayCommand)
        case .failure(let refusal):
            XCTAssertEqual(refusal, .notTheJoinPath(host: "peer", path: "/moved"))
        }

        let join = try url("tcr://peer/join?v=1&nk=AAAABBBBCCCCDDDD")
        XCTAssertEqual(PeerMovedLink.route(join), .join)
        switch PeerMovedLink.invocation(for: join) {
        case .success(let invocation):
            XCTFail(
                "a join link was accepted as a moved link and would have run "
                    + invocation.displayCommand)
        case .failure(let refusal):
            XCTAssertEqual(refusal, .notTheMovedPath(host: "peer", path: "/join"))
        }
    }

    /// A third path under the same scheme belongs to neither, and the join
    /// handler's own sentence for an unknown `tcr://` link is what says so.
    func testAnUnknownTcrPathRoutesToNeither() throws {
        XCTAssertEqual(PeerMovedLink.route(try url("tcr://peer/leave?v=1")), .neither)
        XCTAssertEqual(
            PeerMovedLink.route(try url("tcrbar://check-for-updates")), .neither)
        XCTAssertEqual(PeerMovedLink.route(try url("https://example.com/moved")), .neither)
    }

    /// A trailing slash is the same link to anyone pasting one, on both paths.
    func testATrailingSlashStillRoutes() throws {
        XCTAssertEqual(PeerMovedLink.route(try url("tcr://peer/moved/?v=1&r=AAAA")), .moved)
        XCTAssertEqual(PeerMovedLink.route(try url("tcr://peer/join/?v=1&nk=AAAA")), .join)
    }

    // MARK: What is run

    /// **The preview invocation carries no `--yes` flag.**
    ///
    /// Without it `tcr peer moved open` reads the link and writes nothing,
    /// which is what makes the alert a decision instead of a notice about a
    /// change that already happened. A flag that crept onto the first run
    /// would apply the link before anyone saw what was in it.
    func testThePreviewInvocationCarriesNoYesFlag() throws {
        let link = try url("tcr://peer/moved?v=1&r=AAAABBBBCCCCDDDD")
        let preview = try XCTUnwrap(try? PeerMovedLink.invocation(for: link).get())
        XCTAssertEqual(preview.arguments, ["peer", "moved", "open", "--stdin"])
        XCTAssertFalse(preview.arguments.contains("--yes"))

        let applying = try XCTUnwrap(try? PeerMovedLink.invocation(for: link, apply: true).get())
        XCTAssertEqual(applying.arguments, ["peer", "moved", "open", "--stdin", "--yes"])
    }

    /// **A refusal sentence names no Mac and no address.**
    ///
    /// A link that opens against nothing here must teach whoever pasted it
    /// nothing about who this Mac knows, or a link forwarded into the wrong
    /// group chat becomes a way to ask that question. The sentences also never
    /// echo the string that arrived: its shape goes to the log, not into a
    /// person's face as if the paste were the problem.
    func testARefusalSentenceNamesNoPeerAndNoAddress() throws {
        for refusal in Self.everyRefusal {
            let sentence = PeerMovedLink.sentence(for: refusal)
            XCTAssertFalse(sentence.isEmpty, "a refusal with no sentence is a silent handler")
            for leak in ["192.0.2.7", "41234", "198.51.100.9", "studio-mac", "tcr-4b8we1r0zp"] {
                XCTAssertFalse(
                    sentence.contains(leak),
                    "the refusal sentence carries \(leak): \(sentence)")
            }
        }
    }

    /// Every case of ``PeerMovedLink/Refusal``, each built from a URL that
    /// carries an address and a name, so a sentence that echoed its input
    /// would be caught.
    ///
    /// The `switch` is what keeps this list whole: adding a case to the enum
    /// stops this file compiling until the case is sampled here too.
    private static let everyRefusal: [PeerMovedLink.Refusal] = {
        let samples: [PeerMovedLink.Refusal] = [
            .notOurScheme("https"),
            .notTheMovedPath(host: "192.0.2.7", path: "/studio-mac/41234"),
            .carriesNoRecord,
        ]
        for sample in samples {
            switch sample {
            case .notOurScheme, .notTheMovedPath, .carriesNoRecord: break
            }
        }
        return samples
    }()

    /// The WHOLE URL goes on stdin, because `tcr` owns what a link means. An
    /// app that pulled `r` out and re-spelled it would be a second decider
    /// about the same string.
    func testTheWholeLinkIsWhatGoesOnStdin() throws {
        let link = try url("tcr://peer/moved?v=1&r=AAAABBBBCCCCDDDD")
        let invocation = try XCTUnwrap(try? PeerMovedLink.invocation(for: link).get())
        XCTAssertEqual(invocation.stdin, "tcr://peer/moved?v=1&r=AAAABBBBCCCCDDDD")
    }

    /// And never into argv. Anyone holding the record can re-apply it while it
    /// is still good, and argv is readable by every process on this Mac.
    func testTheSealedRecordIsNeverInArgv() throws {
        let link = try url("tcr://peer/moved?v=1&r=AAAABBBBCCCCDDDD")
        for apply in [false, true] {
            let invocation = try XCTUnwrap(
                try? PeerMovedLink.invocation(for: link, apply: apply).get())
            XCTAssertFalse(invocation.secretIsInArgv)
            XCTAssertFalse(invocation.displayCommand.contains("AAAA"))
            for argument in invocation.arguments {
                XCTAssertFalse(
                    argument.contains("AAAA"),
                    "the sealed record reached argv, where `ps` shows it to every process here")
            }
        }
    }

    /// A link carrying nothing sealed has nothing for `tcr` to open, so it is
    /// refused with its reason rather than run.
    func testALinkWithNoRecordIsRefusedWithItsReason() throws {
        switch PeerMovedLink.invocation(for: try url("tcr://peer/moved?v=1")) {
        case .success:
            XCTFail("a link with no sealed record was accepted")
        case .failure(let refusal):
            XCTAssertEqual(refusal, .carriesNoRecord)
            XCTAssertTrue(
                PeerMovedLink.sentence(for: refusal).contains("nothing to open"))
        }
    }

    /// An empty value is the same state as an absent one: `?r=` is a link a
    /// chat client cut, not a record.
    func testAnEmptyRecordValueIsTreatedAsAbsent() throws {
        switch PeerMovedLink.invocation(for: try url("tcr://peer/moved?v=1&r=")) {
        case .success: XCTFail("an empty record was accepted")
        case .failure(let refusal): XCTAssertEqual(refusal, .carriesNoRecord)
        }
    }

    /// `PeerCommand.moved(open:apply:)` is the app's entry point and answers
    /// the same way, so no caller can pipe an arbitrary URL into `tcr`.
    func testTheCommandFactoryRefusesAnArbitraryUrl() throws {
        switch PeerCommand.moved(open: try url("https://example.com/moved?r=AAAA")) {
        case .success:
            XCTFail("an https URL was accepted as a moved link")
        case .failure(let refusal):
            XCTAssertEqual(refusal, .notOurScheme("https"))
        }
    }

    /// The peer id is not a secret, so `mint` is plain argv, and the verb is
    /// spelled in one place rather than at a button.
    func testTheMintFactoryPutsThePeerOnArgv() {
        XCTAssertEqual(
            PeerCommand.moved(mint: "tcr-example"), ["peer", "moved", "mint", "tcr-example"])
    }

    /// What may be logged: the shape, and whether anything sealed rode along.
    /// Never the record.
    func testTheLoggableFormCarriesNoRecord() throws {
        let redacted = PeerMovedLink.redacted(
            try url("tcr://peer/moved?v=1&r=AAAABBBBCCCCDDDD"))
        XCTAssertEqual(redacted, "tcr://peer/moved with a sealed record")
        XCTAssertFalse(redacted.contains("AAAA"))
        XCTAssertEqual(
            PeerMovedLink.redacted(try url("tcr://peer/moved?v=1")),
            "tcr://peer/moved with nothing sealed")
    }

    // MARK: What a run answered

    /// The exit code is the whole of what this app reads. `tcr` decides
    /// whether a link is good and for whom; its words are carried through
    /// untouched.
    func testACleanReadOffersToKeepAndCarriesTheCliWordsUntouched() {
        let printed = "peer moved: would add 192.0.2.7:41234\npeer moved: nothing written"
        let answer = PeerMovedLink.answer(exitCode: 0, stdout: printed, stderr: "")
        XCTAssertEqual(answer, .clean(printed))
        XCTAssertTrue(answer.isClean)
        XCTAssertEqual(answer.lines, printed)
    }

    /// **A refusal never becomes an offer.** Whatever `tcr` refused, the alert
    /// has a sentence and a stop in it, and no button that keeps, pairs or
    /// trusts anything: a link forwarded into the wrong chat would otherwise
    /// put a trust prompt in front of someone the sender never meant to ask.
    func testARefusedReadOffersNothing() {
        let said = "peer moved: this link was not meant for this Mac"
        let answer = PeerMovedLink.answer(exitCode: 1, stdout: "", stderr: said)
        XCTAssertEqual(answer, .refused(said))
        XCTAssertFalse(answer.isClean)
        XCTAssertEqual(answer.lines, said)
    }

    /// A failure that printed only on stdout still shows `tcr`'s own words,
    /// and one that printed nothing at all says so in this app's words rather
    /// than opening an empty alert.
    func testAFailureWithNothingOnStderrStillSaysSomething() {
        XCTAssertEqual(
            PeerMovedLink.answer(exitCode: 2, stdout: "peer moved: cut short", stderr: ""),
            .refused("peer moved: cut short"))
        let silent = PeerMovedLink.answer(exitCode: 2, stdout: "", stderr: "")
        XCTAssertFalse(silent.isClean)
        XCTAssertTrue(silent.lines.contains("exit 2"))
    }

    /// An exit of 0 with nothing printed is a refusal too: an alert with a
    /// blank body asks a person to agree to nothing.
    func testACleanRunThatPrintedNothingIsNotAnOffer() {
        let answer = PeerMovedLink.answer(exitCode: 0, stdout: "  \n", stderr: "")
        XCTAssertFalse(answer.isClean)
        XCTAssertTrue(answer.lines.contains("nothing to keep"))
    }
}
