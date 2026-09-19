import XCTest

@testable import TcrBarCore

/// The `tcr://` share link: what the app may run when one arrives, and what it
/// may say about it.
///
/// The keys in these fixtures are obviously fake (`AAAA…`, `BBBB…`). A real
/// network key or join key in a test file would be a credential in a public
/// repository, which is the one thing `CLAUDE.md` puts above everything else
/// here.
final class PeerJoinLinkTests: XCTestCase {

    private func url(_ string: String) throws -> URL {
        try XCTUnwrap(URL(string: string))
    }

    /// The WHOLE URL goes on stdin because the CLI owns what a link means.
    /// A panel that unpacked `nk` and
    /// `jk` and re-spelled them would be a second parser of the same string.
    func testTheWholeLinkIsWhatGoesOnStdin() throws {
        let link = try url("tcr://peer/join?v=1&nk=AAAABBBBCCCCDDDD&jk=EEEEFFFF")
        let invocation = try XCTUnwrap(try? PeerJoinLink.invocation(for: link).get())
        XCTAssertEqual(invocation.arguments, ["peer", "join", "--stdin"])
        XCTAssertEqual(invocation.stdin, "tcr://peer/join?v=1&nk=AAAABBBBCCCCDDDD&jk=EEEEFFFF")
    }

    /// And never into argv. `nk` and `jk` are credentials, and argv is
    /// readable by every process on this Mac through `ps`.
    func testTheSecretIsNeverInArgv() throws {
        let link = try url("tcr://peer/join?v=1&nk=AAAABBBBCCCCDDDD")
        let invocation = try XCTUnwrap(try? PeerJoinLink.invocation(for: link).get())
        XCTAssertFalse(invocation.secretIsInArgv)
        XCTAssertFalse(invocation.displayCommand.contains("AAAA"))
        for argument in invocation.arguments {
            XCTAssertFalse(
                argument.contains("AAAA"),
                "the network key reached argv, where `ps` shows it to every process here")
        }
    }

    /// A link with only a network key is valid: decision row 11 says a link
    /// with only `nk` is as durable as the key, and brings a Mac onto the mesh
    /// without enrolling it with anybody.
    func testANetworkKeyAloneIsAValidLink() throws {
        let link = try url("tcr://peer/join?v=1&nk=AAAABBBBCCCCDDDD")
        XCTAssertNoThrow(try PeerJoinLink.invocation(for: link).get())
    }

    func testAJoinKeyAloneIsAValidLink() throws {
        let link = try url("tcr://peer/join?v=1&jk=EEEEFFFF")
        XCTAssertNoThrow(try PeerJoinLink.invocation(for: link).get())
    }

    /// The app's OWN scheme is not this one. `tcrbar://check-for-updates` is
    /// the CLI asking the app to act; `tcr://peer/join` is a person pasting a
    /// credential. Running one as the other would pipe an update check into
    /// `peer join`.
    func testTheAppsOwnSchemeIsRefused() throws {
        let refusal = try assertRefusal(url("tcrbar://check-for-updates"))
        XCTAssertEqual(refusal, .notOurScheme("tcrbar"))
    }

    func testAnotherTcrPathIsRefused() throws {
        let refusal = try assertRefusal(url("tcr://peer/leave?v=1&nk=AAAA"))
        XCTAssertEqual(refusal, .notTheJoinPath(host: "peer", path: "/leave"))
    }

    /// A link carrying neither key sets nothing, so piping it to `tcr` would
    /// be a press that cannot work. Refused with the reason, not run.
    func testALinkWithNoKeyIsRefusedWithItsReason() throws {
        let refusal = try assertRefusal(url("tcr://peer/join?v=1"))
        XCTAssertEqual(refusal, .carriesNoKey)
        XCTAssertTrue(PeerJoinLink.sentence(for: refusal).contains("nothing to join"))
    }

    /// An empty value is the same state as an absent one: `?nk=` is a link
    /// somebody's chat client truncated, not a key.
    func testAnEmptyKeyValueIsTreatedAsAbsent() throws {
        XCTAssertEqual(try assertRefusal(url("tcr://peer/join?v=1&nk=")), .carriesNoKey)
    }

    /// A trailing slash is the same link to anyone pasting one.
    func testATrailingSlashStillJoins() throws {
        XCTAssertNoThrow(
            try PeerJoinLink.invocation(for: try url("tcr://peer/join/?v=1&nk=AAAA")).get())
    }

    /// What may be logged: the shape, and WHICH keys rode along, never a key,
    /// and never the URL. The handler has to say something, because a URL that
    /// silently does nothing is indistinguishable from a broken handler.
    func testTheLoggableFormCarriesNoKeyMaterial() throws {
        let redacted = PeerJoinLink.redacted(
            try url("tcr://peer/join?v=1&nk=AAAABBBBCCCCDDDD&jk=EEEEFFFF"))
        XCTAssertEqual(redacted, "tcr://peer/join with a network key and a join key")
        XCTAssertFalse(redacted.contains("AAAA"))
        XCTAssertFalse(redacted.contains("EEEE"))
    }

    func testTheLoggableFormNamesOnlyTheKeysThatWereThere() throws {
        XCTAssertEqual(
            PeerJoinLink.redacted(try url("tcr://peer/join?v=1&nk=AAAA")),
            "tcr://peer/join with a network key")
        XCTAssertEqual(
            PeerJoinLink.redacted(try url("tcr://peer/join?v=1")),
            "tcr://peer/join with no key")
    }

    /// `PeerCommand.join(link:)` is the app's entry point and answers the same
    /// way, so no caller can pipe an arbitrary URL into `tcr`.
    func testTheCommandFactoryRefusesAnArbitraryUrl() throws {
        switch PeerCommand.join(link: try url("https://example.com/join?nk=AAAA")) {
        case .success:
            XCTFail("an https URL was accepted as a join link")
        case .failure(let refusal):
            XCTAssertEqual(refusal, .notOurScheme("https"))
        }
    }

    // MARK: The confirmation sheet's pure logic

    /// A link's own credential never appears in the fingerprint: hashed, not
    /// excerpted, or a "fingerprint" would just be the key with fewer
    /// characters, the exact leak ``PeerJoinLink`` exists to close.
    func testTheFingerprintNeverContainsTheKey() throws {
        let link = try url("tcr://peer/join?v=1&nk=AAAABBBBCCCCDDDD")
        let fingerprint = try XCTUnwrap(PeerJoinLink.fingerprint(for: link))
        XCTAssertFalse(fingerprint.contains("AAAA"))
        XCTAssertFalse(fingerprint.contains("BBBB"))
    }

    /// The SAME key hashes to the SAME fingerprint every time, which is the
    /// whole point: it lets an operator compare it against the sender's own
    /// screen. A different key must read differently, or the compare proves
    /// nothing.
    func testTheFingerprintIsStableAndDistinguishesKeys() throws {
        let first = PeerJoinLink.fingerprint(for: try url("tcr://peer/join?v=1&nk=AAAABBBB"))
        let again = PeerJoinLink.fingerprint(for: try url("tcr://peer/join?v=1&nk=AAAABBBB"))
        let other = PeerJoinLink.fingerprint(for: try url("tcr://peer/join?v=1&nk=ZZZZYYYY"))
        XCTAssertEqual(first, again)
        XCTAssertNotEqual(first, other)
    }

    /// `nk` wins when a link carries both, because a link with an `nk` sets
    /// the office network key, which is the more consequential of the two,
    /// the same precedence `docs/peers.md` gives a spent `jk` (it still sets
    /// `nk`).
    func testTheFingerprintPrefersTheNetworkKeyOverTheJoinKey() throws {
        let nkOnly = PeerJoinLink.fingerprint(for: try url("tcr://peer/join?v=1&nk=AAAABBBB"))
        let both = PeerJoinLink.fingerprint(
            for: try url("tcr://peer/join?v=1&nk=AAAABBBB&jk=ZZZZYYYY"))
        XCTAssertEqual(nkOnly, both)
    }

    /// No key at all (a link this app would already have refused before
    /// reaching a confirmation) fingerprints as `nil` rather than hashing the
    /// empty string, which would print as a fingerprint that means nothing.
    func testNoKeyFingerprintsAsNil() throws {
        XCTAssertNil(PeerJoinLink.fingerprint(for: try url("tcr://peer/join?v=1")))
    }

    /// The body names what rode along and never states a fact it cannot back:
    /// with no existing key, it says nothing about replacing one.
    func testConfirmationBodyNamesTheShapeAndFingerprintButNoReplacementWithNoExistingKey() throws {
        let link = try url("tcr://peer/join?v=1&nk=AAAABBBB")
        let body = PeerJoinLink.confirmationBody(for: link, hasExistingKey: false)
        XCTAssertTrue(body.contains("network key"))
        XCTAssertTrue(body.contains("Fingerprint:"))
        XCTAssertFalse(body.contains("already has a network key"))
    }

    /// With an existing key, the body says so: the one fact this app can
    /// state without re-deriving `tcr`'s own refusal logic.
    func testConfirmationBodyNamesAnExistingKeyWhenThereIsOne() throws {
        let link = try url("tcr://peer/join?v=1&nk=AAAABBBB")
        let body = PeerJoinLink.confirmationBody(for: link, hasExistingKey: true)
        XCTAssertTrue(body.contains("already has a network key"))
    }

    // MARK: `tcr peer network-key show`'s own words

    func testNetworkKeyShowSetReadsAsTrue() {
        XCTAssertTrue(PeerJoinLink.networkKeyIsSet(output: "peer network-key: set\n"))
    }

    func testNetworkKeyShowNotSetReadsAsFalse() {
        XCTAssertFalse(PeerJoinLink.networkKeyIsSet(output: "peer network-key: not-set\n"))
    }

    /// Anything this build does not recognize, a refused verb's error text,
    /// an older `tcr`'s silence, empty output from a `tcr` this app could not
    /// even resolve, reads as "not set" rather than raising an alarm this
    /// app cannot back up.
    func testUnrecognizedNetworkKeyOutputReadsAsFalse() {
        XCTAssertFalse(PeerJoinLink.networkKeyIsSet(output: ""))
        XCTAssertFalse(PeerJoinLink.networkKeyIsSet(output: "error: unrecognized subcommand"))
    }

    private func assertRefusal(
        _ url: URL, file: StaticString = #filePath, line: UInt = #line
    ) throws -> PeerJoinLink.Refusal {
        switch PeerJoinLink.invocation(for: url) {
        case .success(let invocation):
            XCTFail(
                "\(url.absoluteString) was accepted and would have run "
                    + invocation.displayCommand, file: file, line: line)
            throw PeerJoinLink.Refusal.carriesNoKey
        case .failure(let refusal):
            return refusal
        }
    }
}
