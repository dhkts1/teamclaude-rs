import XCTest

@testable import TcrBarCore

/// Decoding `tcr peer ls --json`, against the shape `src/main.rs`'s
/// `PeerLsJson` actually writes.
///
/// This suite exists because the document moved into `TcrBarCore`.
/// While it lived in the executable target the only available check was a grep
/// over the source for a field NAME, which passes whether or not the name
/// matches the producer's, and passes whether or not the nesting does. Every
/// payload below is shaped like the real one: `camelCase` keys, `pending` /
/// `blocked` / `muted` as ROWS beside their counts, `caps` as an object,
/// `reason` kebab-case, and no account email or UUID anywhere (this repository
/// is public, so the labels are `alice` and the addresses are private-range).
final class PeerListDocumentTests: XCTestCase {

    private func decode(_ json: String) throws -> PeerListDocument {
        try JSONDecoder().decode(PeerListDocument.self, from: Data(json.utf8))
    }

    /// Whether this Mac is on a network at all: a third state beside
    /// "looking" and "off", and the one the Find card had no words for.
    ///
    /// Three values and not two. `false` is a Mac with no Wi-Fi and no cable,
    /// where "Looking" is a false claim and the card under it explaining that
    /// only Macs on this network can appear is the wrong sentence entirely.
    /// ABSENT is every `tcr` shipped so far, which reports nothing of the
    /// kind, and that reads as "not known" and draws exactly what it drew
    /// before, never as "no network".
    func testTheDocumentCarriesWhetherThereIsANetworkAtAll() throws {
        XCTAssertEqual(try decode(#"{"supported":true,"network":false}"#).network, false)
        XCTAssertEqual(try decode(#"{"supported":true,"network":true}"#).network, true)
        XCTAssertNil(
            try decode(#"{"supported":true,"finding":true}"#).network,
            "a tcr that says nothing about interfaces is read as having none, so every "
                + "panel against today's binary draws a no-network card")
    }

    /// **A peer row out of the BUILT binary, pasted verbatim.**
    ///
    /// Every other payload in this file was written from the producer's source.
    /// This one was produced: a temp peers file with one pinned Mac, the `tcr`
    /// this tree builds, `peer ls --json`, one row copied out of the output
    /// with nothing but the node id shortened. It is here because the source
    /// and the binary disagreed and only the binary was right: the panel read
    /// `id` and `name`, the row says `node` and `label`, so every pinned Mac
    /// decoded to a nil id, a nil name and `trusted: false`.
    ///
    /// Watch it fail by dropping either fallback in `PeerEntry.init(from:)`:
    /// the id or the name goes nil while every other assertion here passes.
    func testDecodesAPeerRowTheBuiltBinaryProduced() throws {
        let document = try decode(
            """
            {"supported":true,"peers":[
              {"addedAt":1,
               "allow":{"acceptMove":false,"allowDisclose":false,"carry":true,
                        "control":{"briefs":false,"diag":false,"lendable":false},
                        "gateway":false,"inspect":true,"relay":false},
               "ended":false,
               "endpoints":[{"addr":"192.0.2.7:41234","kind":"direct",
                             "observedAtMs":2,"source":"paired"}],
               "label":"studio-mac","lend":[],
               "node":"0000000000000000000000000000000000000000000000000000",
               "until":null}],
             "pending":[],"pendingCount":0,"blocked":[],"blockedCount":0,
             "muted":[],"mutedCount":0,"limited":0}
            """)
        let row = try XCTUnwrap(document.peers.first)
        XCTAssertEqual(
            row.id, "0000000000000000000000000000000000000000000000000000",
            "the pinned key is the row's `node`, which is what every reader matches a Mac by")
        XCTAssertEqual(row.name, "studio-mac", "and the operator's own word for it is `label`")
        XCTAssertEqual(
            row.address, "192.0.2.7:41234",
            "the address comes off the first endpoint that has one")
        XCTAssertTrue(
            row.trusted,
            "every row in this document is a pinned Mac: the file IS the record of what this "
                + "Mac trusts, and there is no `trusted` key on it to say otherwise")
        XCTAssertTrue(row.carries, "`allow.carry` is the flag, spelled as the peers file spells it")
        // This row's `allowDisclose` is `false` and its `inspect` is `true`:
        // the two flags disagree on purpose, so a decoder that fell back to
        // the wrong one could not pass both this and the assertion above.
        // `serves` answers "may that Mac serve on OUR accounts", which is
        // `allow.allowDisclose` (`config.rs`'s own doc, `status.rs:790`);
        // `allow.inspect` is the other direction of the pair, "may THAT Mac
        // read what WE forward it", a different question this field does not
        // answer.
        XCTAssertFalse(row.serves, "`allow.allowDisclose` is the flag, not `allow.inspect`")
    }

    /// **The status block's spellings still win where they are sent.**
    ///
    /// `PeerStatusRow` (`src/status.rs`) says `id`, `name`, `address`,
    /// `trusted`, and the fallbacks above must not shadow them. The control is
    /// a row carrying BOTH spellings with different values: the first key is
    /// the answer, or the fix for one producer is a regression for the other.
    func testTheStatusSpellingsWinOverTheLsFallbacks() throws {
        let document = try decode(
            """
            {"supported":true,"peers":[
              {"id":"from-status","node":"from-ls",
               "name":"status-name","label":"ls-label",
               "address":"192.0.2.9:1","endpoints":[{"addr":"192.0.2.8:2","kind":"direct"}],
               "trusted":false,"carries":false,"serves":false,
               "allow":{"carry":true,"allowDisclose":true}}]}
            """)
        let row = try XCTUnwrap(document.peers.first)
        XCTAssertEqual(row.id, "from-status")
        XCTAssertEqual(row.name, "status-name")
        XCTAssertEqual(row.address, "192.0.2.9:1")
        XCTAssertFalse(
            row.trusted,
            "a producer that says `trusted: false` is answered, never overridden by the "
                + "presence of a `node`")
        XCTAssertFalse(row.carries, "and the same for the two grant flags")
        XCTAssertFalse(row.serves)
    }

    /// The whole of decision rows 10 and 11, in one document, as `tcr peer ls
    /// --json` writes it today (`src/main.rs:1194-1209`).
    func testDecodesTheAdmissionBlocksTheBinaryWritesToday() throws {
        let document = try decode(
            """
            {"supported":true,"peers":[],
             "pending":[{"addr":"10.0.1.24","instanceId":"8f2c1ad63b0e4471",
                         "proposedName":"loft-mini","wireVersion":1,
                         "firstSeenMs":1700000000000,"lastSeenMs":1700000003000}],
             "pendingCount":1,
             "blocked":[{"addr":"10.0.1.99","sinceMs":1699999000000,"reason":"blocked"}],
             "blockedCount":1,
             "muted":[{"addr":"10.0.1.55","untilMs":1700003600000}],
             "mutedCount":1,
             "limited":0,
             "caps":{"foundRows":12,"foundPerAddress":2,"pending":8,
                     "knockIntervalMs":10000,"knockBurst":3,
                     "unauthenticatedSockets":16}}
            """)

        XCTAssertEqual(document.pending.count, 1)
        XCTAssertEqual(document.pending.first?.instanceId, "8f2c1ad63b0e4471")
        XCTAssertEqual(document.pending.first?.proposedName, "loft-mini")
        XCTAssertEqual(document.pending.first?.addr, "10.0.1.24")
        XCTAssertEqual(document.blocked.first?.addr, "10.0.1.99")
        XCTAssertEqual(document.blocked.first?.reason, .blocked)
        XCTAssertNil(document.blocked.first?.key, "a knock reveals no static key")
        XCTAssertEqual(document.muted.first?.untilMs, 1_700_003_600_000)
        XCTAssertEqual(document.caps?.foundRows, 12)
        XCTAssertEqual(document.caps?.knockIntervalMs, 10000)
        XCTAssertEqual(document.caps?.unauthenticatedSockets, 16)
    }

    /// The producer's count WINS over the row count. That is the whole reason
    /// `src/main.rs:1133-1141` sends both: when rows are held back the two
    /// disagree on purpose, and a panel that recomputed the count from the
    /// array it was handed could never draw "8 held, 2 shown".
    func testACountTheProducerSentBeatsTheRowCount() throws {
        let document = try decode(
            """
            {"pending":[{"addr":"10.0.1.24","instanceId":"aa","wireVersion":1}],
             "pendingCount":8}
            """)
        XCTAssertEqual(document.pending.count, 1)
        XCTAssertEqual(document.pendingCount, 8)
    }

    /// And falls back to the row count only when the key is absent, so an
    /// older `tcr` still gets a footer that says something true.
    func testAnAbsentCountFallsBackToTheRows() throws {
        let document = try decode(
            """
            {"muted":[{"addr":"10.0.1.55","untilMs":1},{"addr":"10.0.1.56","untilMs":2}]}
            """)
        XCTAssertEqual(document.mutedCount, 2)
    }

    /// A ban reason a newer `tcr` invents must not blank the Blocked list.
    /// `.unknown` and a sentence that says so, not a thrown decode.
    func testAnUnknownBanReasonDecodesRatherThanFailingTheDocument() throws {
        let document = try decode(
            """
            {"blocked":[{"addr":"10.0.1.99","sinceMs":1,"reason":"some-future-reason",
                         "key":"NNNNNNNNNNNNNNNNNNNNNNNNNNNNNNNNNNNNNNNNNNNNNNNNNNNN"}]}
            """)
        XCTAssertEqual(document.blocked.first?.reason, .unknown)
        XCTAssertNotNil(document.blocked.first?.key)
        XCTAssertTrue(
            document.blocked.first?.reason.sentence.contains("cannot say why") == true)
    }

    /// The `tcr` IN THIS TREE sends none of `finding`, `sharing`,
    /// `answeringOn` or the six Settings readouts (`src/main.rs:1194`), so a
    /// document without them has to decode and the panel has to draw the
    /// absence. A required key here would collapse the tab against the very
    /// binary it ships beside.
    func testTheDocumentThisTreesBinaryWritesDecodesWithoutTheOptionalKeys() throws {
        let document = try decode(#"{"supported":true,"peers":[],"limited":0}"#)
        XCTAssertTrue(document.supported)
        XCTAssertFalse(document.finding)
        XCTAssertNil(document.caps)
        XCTAssertNil(document.nodeId)
        XCTAssertTrue(document.lentTo.isEmpty)
    }

    /// A `lend` array on a peer row, and a `lentTo` map. Neither is in this
    /// tree's `tcr` yet, so this is the panel coding against the shape it
    /// will carry, and the gate that it decodes when it lands.
    /// The shapes are the producer's real wire ones: `id` (never `leaseId`,
    /// see `LendGrant::id`'s `lease_id_hex` rename) and `scope` as
    /// `tcr_peer_wire::LendScope`'s externally tagged JSON (`{"group":"work"}`,
    /// `{"accounts":["alice"]}`), not the CLI's `--scope` string spelling.
    /// This is the exact shape that used to throw a `typeMismatch` out of the
    /// whole `[PeerLendGrant]` array and blank the Peers tab: watch it fail by
    /// reverting `LendScope`'s `Decodable` conformance to a bare `String`
    /// decode.
    func testDecodesTheLendGrantsAndTheLentToMapRequestIsAdding() throws {
        let document = try decode(
            """
            {"peers":[{"id":"studio","name":"studio-mac","trusted":true,
                       "lend":[{"id":"ls-4b1f","scope":{"group":"work"},"window":"7d",
                                "fraction":0.2,"ttl":300,"maxInflight":2,
                                "until":1700006400,"ended":false},
                               {"id":"ls-2e77","scope":{"accounts":["alice"]},"window":"5h",
                                "fraction":0.2,"ttl":300,"maxInflight":2,
                                "until":1699999800,"ended":true}]}],
             "lentTo":{"alice":[{"peer":"attic-nuc","scope":{"group":"work"},"window":"7d",
                                 "fraction":0.2,"ended":false},
                                {"peer":"studio-mac","scope":{"accounts":["alice"]},
                                 "window":"7d_oi","fraction":1.0,"ended":true}]}}
            """)

        let grants = try XCTUnwrap(document.peers.first?.lend)
        XCTAssertEqual(grants.count, 2)
        XCTAssertEqual(grants[0].leaseId, "ls-4b1f")
        XCTAssertEqual(grants[0].scope, .group("work"))
        XCTAssertEqual(grants[0].window, .week)
        XCTAssertEqual(grants[0].maxInFlight, 2)
        XCTAssertEqual(grants[0].until, 1_700_006_400)
        XCTAssertTrue(grants[1].ended)
        XCTAssertEqual(grants[1].scope, .accounts(["alice"]))

        let lent = try XCTUnwrap(document.lentTo["alice"])
        XCTAssertEqual(lent.map(\.peer), ["attic-nuc", "studio-mac"])
        XCTAssertEqual(lent[0].scope, .group("work"))
        XCTAssertEqual(lent[1].scope, .accounts(["alice"]))
        XCTAssertEqual(lent[1].window, .fableWeek)
        XCTAssertEqual(lent[1].fraction, 1.0)
        XCTAssertFalse(lent[0].ended)
        XCTAssertTrue(lent[1].ended)
    }

    /// **The shape the CLI writes TODAY.** `src/peer/config.rs:414`'s
    /// `LendGrant` is `{window, fraction, ttlS, maxInflight}` under
    /// `rename_all = "camelCase"`, and this decoder also accepts `ttl`. Both
    /// spellings decode to the same field, the alternative is a panel that
    /// shows the 300 s DEFAULT for every lease whose real ttl it did not read,
    /// which looks exactly like a lease that really is 300 s.
    func testALeaseTtlDecodesUnderEitherSpellingTheProducerUses() throws {
        let brief = try decode(
            """
            {"peers":[{"trusted":true,"lend":[{"id":"ls-1","scope":"all",
                        "window":"7d","fraction":0.1,"ttl":600,"maxInflight":2}]}]}
            """)
        XCTAssertEqual(brief.peers.first?.lend.first?.ttlSeconds, 600)

        let today = try decode(
            """
            {"peers":[{"trusted":true,"lend":[{"id":"ls-1","scope":"all",
                        "window":"7d","fraction":0.1,"ttlS":600,"maxInflight":2}]}]}
            """)
        XCTAssertEqual(
            today.peers.first?.lend.first?.ttlSeconds, 600,
            "a lease written with the producer's own `ttlS` fell back to the 300 s default, "
                + "which reads on screen as a lease that really renews every 300 s")
    }

    /// A window this build does not know must not fail the DOCUMENT.
    ///
    /// The wire carries the same arm for the same reason
    /// (`crates/tcr-peer-wire/src/lib.rs:522`: "parse survives, handler
    /// refuses"). A throwing enum would collapse the whole Peers surface to
    /// "tcr could not be read" because one lease named a fourth allowance,
    /// and it must not silently become one of the three either, so it lends
    /// nothing.
    func testAnUnknownWindowSurvivesTheDecodeAndLendsNothing() throws {
        let document = try decode(
            """
            {"peers":[{"trusted":true,"lend":[{"id":"ls-1","scope":"all",
                        "window":"30d_future","fraction":0.1,"ttl":300}]}],
             "lentTo":{"alice":[{"peer":"attic-nuc","window":"30d_future","fraction":0.1}]}}
            """)
        XCTAssertEqual(document.peers.first?.lend.first?.window, .unknown)
        XCTAssertEqual(document.lentTo["alice"]?.first?.window, .unknown)
        XCTAssertEqual(LeaseTerms.standard(for: .unknown).fraction, 0)
        XCTAssertFalse(
            PeerLeaseWindow.allCases.contains(.unknown),
            "an allowance this build cannot name is offered in a picker, so an operator can "
                + "lend against a window nobody here understands")
    }

    /// A scope shape this build cannot name (a future fourth variant of
    /// `tcr_peer_wire::LendScope`, an externally tagged object with neither
    /// `group` nor `accounts`) must not throw the row, and its own lease id,
    /// away: the row still draws, rather than the whole `[PeerLendGrant]`
    /// array, and every OTHER lease on the row with it, failing to decode.
    ///
    /// It is `.unknown`, not `.all`: this used to fall back to `all`, the
    /// same default an absent `scope` key means, which could not tell "no
    /// scope was written" from "a scope was written and this build could not
    /// read it". The second case silently WIDENED a lease the operator had
    /// narrowed to one group or account into a grant covering every account
    /// this Mac holds. `.unknown` is honest about the difference and
    /// `LeaseDraft.refusal` refuses to save one rather than send `--scope
    /// all` for a scope nobody here actually read.
    func testAnUnrecognizedScopeShapeFallsBackToUnknownRatherThanLosingTheRowOrWidening() throws {
        let document = try decode(
            """
            {"peers":[{"trusted":true,"lend":[{"id":"ls-1","scope":{"tenant":"acme"},
                                               "window":"7d","fraction":0.1}]}]}
            """)
        let grant = try XCTUnwrap(document.peers.first?.lend.first)
        XCTAssertEqual(grant.leaseId, "ls-1", "the row survives even though its scope did not")
        XCTAssertEqual(grant.scope, .unknown)
        XCTAssertEqual(grant.scopeLabel, "an unknown scope")
        XCTAssertNotEqual(
            grant.scope, .all,
            "a scope this build cannot parse must never read as every account")
    }
}
