import XCTest

@testable import TcrBarCore

/// THE OTHER HALF OF THE PEERS CONTRACT PIN.
///
/// `tests/peer_status.rs`'s `the_committed_peers_fixture_matches_what_the_payload
/// _renders` proves the Rust side still renders these exact bytes; this proves
/// this app can still read them. A key renamed on either side turns one of the
/// two red, and regenerating the fixture to satisfy that one turns the other red
/// in the same breath, the only arrangement in which a silent rename is
/// impossible.
///
/// It is the arrangement this block needed most: thirteen of the sixteen fields
/// the panel decodes had no Rust writer at all, every field here is
/// `decodeIfPresent`, and so the tab rendered an empty row while both suites
/// stayed green.
///
/// Fixtures use obviously-fake names and loopback addresses only, this
/// repository is public.
final class PeersStatusBlockTests: XCTestCase {
    /// `tests/fixtures/peer-status.json`, at the repo root: the SAME file
    /// `tests/peer_status.rs`'s own gate renders and compares against, never a
    /// copy of it. This suite used to read its own `Fixtures/` copy, hand-kept
    /// in step with that one by nothing but a doc comment's word; the two
    /// drifted the moment `status.rs` started writing `id` as the wire form
    /// with a separate `display` key and this copy did not.
    static var fixtureURL: URL {
        let thisFile = URL(fileURLWithPath: #filePath)
        let repoRoot =
            thisFile
            .deletingLastPathComponent()  // PeersStatusBlockTests.swift -> TcrBarTests
            .deletingLastPathComponent()  // TcrBarTests -> Tests
            .deletingLastPathComponent()  // Tests -> apps/macos
            .deletingLastPathComponent()  // apps/macos -> apps
            .deletingLastPathComponent()  // apps -> repo root
        return repoRoot.appendingPathComponent("tests/fixtures/peer-status.json")
    }

    private func fixtureRows() throws -> [PeerListDocument.PeerEntry] {
        let url = Self.fixtureURL
        XCTAssertTrue(
            FileManager.default.fileExists(atPath: url.path),
            "the committed peers fixture must be reachable from the Swift package: \(url.path)"
        )
        return try JSONDecoder().decode(
            [PeerListDocument.PeerEntry].self, from: try Data(contentsOf: url))
    }

    /// The derived row: every field the serving process can answer from a file
    /// arrives, and every field nothing measures yet is `nil` rather than zero.
    func testTheDerivedRowCarriesWhatIsMeasuredAndNilsWhatIsNot() throws {
        let rows = try fixtureRows()
        XCTAssertEqual(rows.count, 2, "one derived row, one fully measured row")
        let derived = rows[0]

        // The wire form (`id`) and the short form (`display`) are two
        // different strings on the real payload, never the same key twice:
        // decoding `display` into `id` (or vice-versa) both compile and
        // decode, so only an exact-value check on both catches a swap.
        XCTAssertEqual(derived.id, "0W3GE1R70W3GE1R70W3GE1R70W3GE1R70W3GE1R70W3GE1R70W3G")
        XCTAssertEqual(derived.display, "tcr-0W3GE1R70W")
        XCTAssertEqual(derived.name, "studio-mac")
        XCTAssertEqual(derived.address, "127.0.0.1:7749")
        XCTAssertTrue(derived.trusted)
        XCTAssertTrue(derived.carries)
        XCTAssertTrue(derived.serves)
        XCTAssertEqual(derived.lastSeenMs, 1_699_999_700_000)
        XCTAssertEqual(derived.leaseSpent, 0.5)
        XCTAssertEqual(derived.leaseTtlSeconds, 3600)
        XCTAssertEqual(derived.byteCapPerHour, 2_147_483_648)
        // The one rate the ledger can answer, and the reason the block is not
        // all absences: a token lease knows what it granted and how much of it
        // was drawn.
        XCTAssertEqual(derived.tokensPerHour, 250_000)

        // `nil`, not `0`. A zero here reads as a measurement, and the writer
        // for each of these lands with the prober.
        XCTAssertNil(derived.inFlight)
        XCTAssertNil(derived.bytesPerHour)
        XCTAssertEqual(derived.paths.count, 2, "two endpoints, two paths")
        for path in derived.paths {
            XCTAssertEqual(path.kind, .direct)
            XCTAssertNil(path.rttMs, "no writer measures a round trip yet")
            XCTAssertNil(path.lossPct)
        }
        XCTAssertEqual(derived.paths.map(\.endpoint), ["127.0.0.1:7749", "127.0.0.1:7750"])
    }

    /// The measured row, which exists in the fixture for one reason: a key the
    /// derivation never populates is a key a rename could not turn red.
    func testTheMeasuredRowCarriesEveryPerPathFigure() throws {
        let measured = try fixtureRows()[1]
        XCTAssertEqual(measured.name, "attic-nuc")
        XCTAssertEqual(measured.inFlight, 2)
        XCTAssertEqual(measured.bytesPerHour, 41_943_040)
        XCTAssertEqual(measured.tokensPerHour, 90_000)
        XCTAssertEqual(measured.until, 1_700_003_600)
        XCTAssertEqual(measured.paths.count, 2)

        let direct = measured.paths[0]
        XCTAssertEqual(direct.kind, .direct)
        XCTAssertEqual(direct.rttMs, 18.5)
        XCTAssertEqual(direct.lossPct, 0)
        XCTAssertEqual(direct.bytesPerHour, 41_943_040)
        XCTAssertEqual(direct.lastOkMs, 1_699_999_998_000)

        // The forwarded path, which the row draws differently: a `via` endpoint
        // is a peer id, not a socket address, and it carries no byte figure
        // because the bytes are charged to the Mac that forwards them.
        //
        // The WIRE form, not `tcr-0W3GE1R70W`: `PeerStatusRow.id` (studio-mac's
        // own row, `fixtureRows()[0]`) is the wire form too, and the panel
        // resolves a via endpoint's name by looking it up against that `id`
        // (`peerNames`, `PeersTabV4.swift`). A display-form endpoint could
        // never match.
        let via = measured.paths[1]
        XCTAssertEqual(via.kind, .via)
        XCTAssertEqual(via.endpoint, "0W3GE1R70W3GE1R70W3GE1R70W3GE1R70W3GE1R70W3GE1R70W3G")
        XCTAssertEqual(via.rttMs, 96)
        XCTAssertEqual(via.lossPct, 0.02)
        XCTAssertNil(via.bytesPerHour)
    }

    /// **The forwarded path resolves to the forwarder's real name against the
    /// GOLDEN fixture**, not a hand-built id-to-name map that happens to agree
    /// with itself.
    ///
    /// `PeerPathLineTests` used to hand-build both the via path's `endpoint`
    /// and the `names` dictionary it was resolved against in the same
    /// literal (`"tcr-92hbq5t7yv"` on both sides), which stayed green
    /// through a fixture where `PeerStatusRow.id` and `PathStatus.endpoint`
    /// disagree in FORM, one wire, one display, since a test built that way
    /// never reads either one off a real payload. This builds `names` from
    /// `rows[0]` (studio-mac's own `id`/`name`) the way
    /// `PeersSnapshotBuilder.peerNames` does, and resolves `rows[1]`'s via
    /// path against it.
    func testTheForwardedPathResolvesToTheForwardersNameFromTheGoldenFixture() throws {
        let rows = try fixtureRows()
        let forwarder = rows[0]
        let measured = rows[1]
        let forwarderId = try XCTUnwrap(forwarder.id)
        let forwarderName = try XCTUnwrap(forwarder.name)
        let names = [forwarderId: forwarderName]
        let via = try XCTUnwrap(measured.paths.first { $0.kind == .via })
        XCTAssertEqual(
            PeerFormat.pathLine(via, names: names),
            "via \(forwarderName) · 96 ms · 2% lost")
    }

    /// A kind this build has never heard of draws as itself and never blanks
    /// the tab, the `.unknown(String)` rule `FleetStatus` already applies to
    /// every enum on the wire. A relay path is phase 6's, and a panel that
    /// refused the whole document when one arrived would be a panel that breaks
    /// on an upgrade of the other side.
    func testAnUnknownPathKindDecodesAsItselfRatherThanFailing() throws {
        let json = Data(
            """
            [{"id":"tcr-0W3GE1R70W","trusted":true,
              "paths":[{"endpoint":"127.0.0.1:7749","kind":"relay"}]}]
            """.utf8)
        let rows = try JSONDecoder().decode([PeerListDocument.PeerEntry].self, from: json)
        XCTAssertEqual(rows.first?.paths.first?.kind, .unknown("relay"))
    }

    /// A row from a `tcr` that predates the block has no `paths` key at all and
    /// decodes to an empty list, which is the honest absence: this Mac reported
    /// no way to reach that one. Not a failure, and not one invented path.
    func testARowWithNoPathsKeyDecodesToNoPaths() throws {
        let json = Data(#"[{"id":"tcr-0W3GE1R70W","trusted":true}]"#.utf8)
        let rows = try JSONDecoder().decode([PeerListDocument.PeerEntry].self, from: json)
        XCTAssertEqual(rows.first?.paths, [])
        XCTAssertNil(rows.first?.tokensPerHour)
    }

    // MARK: The sub-line the row draws

    /// A measured direct path names its endpoint, its round trip and its loss.
    func testAMeasuredPathLineNamesTheRoundTripAndTheLoss() {
        let line = PeerFormat.pathLine(
            PeerListDocument.PeerPath(
                endpoint: "127.0.0.1:7751", kind: .direct, rttMs: 18.5, lossPct: 0.02))
        XCTAssertEqual(line, "127.0.0.1:7751 · 19 ms · 2% lost")
    }

    /// A measured ZERO loss is a measurement and says so in words, because "0%
    /// lost" beside a round trip reads as noise while "no loss" reads as a
    /// finding.
    func testAMeasuredZeroLossReadsAsNoLoss() {
        let line = PeerFormat.pathLine(
            PeerListDocument.PeerPath(
                endpoint: "127.0.0.1:7751", kind: .direct, rttMs: 4, lossPct: 0))
        XCTAssertEqual(line, "127.0.0.1:7751 · 4 ms · no loss")
    }

    /// The state every build ships in until the prober lands: the path is known
    /// and nothing about it is measured. The line says so rather than drawing a
    /// zero, which would read as an instantaneous, lossless path.
    func testAnUnmeasuredPathSaysSoRatherThanReadingZero() {
        let line = PeerFormat.pathLine(
            PeerListDocument.PeerPath(endpoint: "127.0.0.1:7749", kind: .direct))
        XCTAssertEqual(line, "127.0.0.1:7749 · not measured")
    }

    /// A forwarded path leads with `via`, because the endpoint is the Mac doing
    /// the forwarding and an operator reading a bare peer id where every other
    /// row shows a socket address has no way to tell which it is.
    func testAForwardedPathLeadsWithVia() {
        let line = PeerFormat.pathLine(
            PeerListDocument.PeerPath(
                endpoint: "tcr-0W3GE1R70W", kind: .via, rttMs: 96, lossPct: nil))
        XCTAssertEqual(line, "via tcr-0W3GE1R70W · 96 ms")
    }

    // MARK: The fold

    /// The file half answers the names and the grants; the live half answers
    /// every measurement. A fold that replaced rows wholesale would blank the
    /// first set on every poll, which is the one outcome the tab cannot have.
    func testTheLiveHalfAddsMeasurementsAndKeepsWhatOnlyTheFileKnows() {
        let file = PeerListDocument(
            finding: true, sharing: true,
            peers: [
                .init(
                    id: "tcr-0W3GE1R70W", name: "studio-mac", address: "127.0.0.1:7749",
                    trusted: true, lastSeenMs: 1_699_999_000_000, carries: true,
                    lend: [
                        .init(
                            leaseId: "ls-4b1f", scope: .group("work"), window: .week,
                            fraction: 0.2, ttlSeconds: 300, maxInFlight: 2)
                    ])
            ])
        let live = PeerListDocument.LivePeersRead(
            supported: true,
            peers: [
                // NO name and NO lend on the live row, deliberately: those are
                // the file's facts, and a fold that replaced the row wholesale
                // would blank both while every measurement still looked right.
                .init(
                    id: "tcr-0W3GE1R70W", address: "127.0.0.1:7750",
                    trusted: true, lastSeenMs: 1_699_999_998_000, carries: true, serves: true,
                    inFlight: 2, tokensPerHour: 90_000,
                    paths: [.init(endpoint: "127.0.0.1:7750", kind: .direct, rttMs: 18.5)])
            ])

        let merged = file.mergingLive(live)
        XCTAssertEqual(merged.peers.count, 1, "a fold never grows a matched row into two")
        let row = merged.peers[0]
        // The live half's, because only a running process can know them.
        XCTAssertEqual(row.inFlight, 2)
        XCTAssertEqual(row.tokensPerHour, 90_000)
        XCTAssertEqual(row.lastSeenMs, 1_699_999_998_000)
        XCTAssertEqual(row.address, "127.0.0.1:7750")
        XCTAssertEqual(row.paths.map(\.endpoint), ["127.0.0.1:7750"])
        XCTAssertTrue(row.serves)
        // The file's, untouched: a lease grant is a line in the peers file and
        // the live half does not report one.
        XCTAssertEqual(row.lend.map(\.leaseId), ["ls-4b1f"])
        XCTAssertEqual(row.name, "studio-mac")
        XCTAssertTrue(merged.sharing, "the fold touches the rows and nothing else")
    }

    /// The fold joins on `id`, and both halves must send the SAME form of it
    /// or nothing ever matches: `tcr peer ls --json` writes the wire `node` id
    /// under `id`/`node` (see `PeerEntry.init(from:)`) and, once
    /// `status --json` carries the wire form there too rather than the
    /// `tcr-…` display one, the two decode to identical strings. Decoded from
    /// real wire JSON on both sides, not built with the memberwise init,
    /// which would bypass the key mapping this proves.
    func testMergingLiveJoinsOnTheSharedWireFormId() throws {
        let wireId = "0000000000000000000000000000000000000000000000000001"
        let file = try JSONDecoder().decode(
            PeerListDocument.self,
            from: Data(
                """
                {"peers":[{"node":"\(wireId)","label":"studio-mac","lend":[]}]}
                """.utf8))
        let live = try PeerListDocument.decodeLivePeers(
            Data(
                """
                {"supported":true,"peers":[
                  {"id":"\(wireId)","trusted":true,"inFlight":1}
                ]}
                """.utf8))

        let merged = file.mergingLive(live)
        XCTAssertEqual(
            merged.peers.count, 1,
            "one pinned Mac reported under the same id by both producers must fold into one "
                + "row, not one row per producer")
        XCTAssertEqual(merged.peers.first?.name, "studio-mac", "the file's own fact survives")
        XCTAssertEqual(merged.peers.first?.inFlight, 1, "the live half's own fact is folded in")
    }

    /// A `tcr` with no live-peers verb: the tab keeps every row it read from
    /// the file and draws no paths. Forward compatibility, not a failure.
    ///
    /// Every ROW is left alone. One thing is recorded: whether the live half
    /// answered at all, which used to be dropped here and is the one fact
    /// that tells "this Mac looked and found no way there" from "nothing
    /// looked". The whole-document equality this test used to assert could
    /// not distinguish the two, which is what let the tab word both the same.
    func testAnUnsupportedLiveReadLeavesEveryRowAloneAndRecordsThatItDidNotAnswer() {
        let file = PeerListDocument(
            peers: [.init(id: "tcr-0W3GE1R70W", name: "studio-mac", trusted: true)])

        let unread = file.mergingLive(.unsupported)
        XCTAssertEqual(unread.peers, file.peers, "a row changed on a read that answered nothing")
        XCTAssertEqual(
            unread.liveAnswered, false,
            "the tab cannot tell an unread live half from a measured absence again")

        let answeredWithNoRows = file.mergingLive(.init(supported: true, peers: []))
        XCTAssertEqual(answeredWithNoRows.peers, file.peers)
        XCTAssertEqual(
            answeredWithNoRows.liveAnswered, true,
            "a live half that answered and reported no peers is a measurement, not silence")
        XCTAssertNil(
            file.liveAnswered,
            "a document nobody merged a live read into claims one either way")
    }

    /// A pinned Mac the file half never listed is the one Mac an operator is
    /// certain to look for, so it is appended rather than dropped.
    func testALiveRowWithNoFileRowIsAppended() {
        let file = PeerListDocument(
            peers: [.init(id: "tcr-0W3GE1R70W", name: "studio-mac", trusted: true)])
        let merged = file.mergingLive(
            .init(
                supported: true,
                peers: [.init(id: "tcr-144GJ28914", name: "attic-nuc", trusted: true)]))
        XCTAssertEqual(merged.peers.map(\.id), ["tcr-0W3GE1R70W", "tcr-144GJ28914"])
    }

    /// One row this build cannot read must not destroy the others: it is
    /// COUNTED, the rule the accounts decode already follows.
    func testAnUnreadableLiveRowIsCountedAndTheRestSurvive() throws {
        let json = Data(
            """
            {"supported": true, "peers": [
              {"id":"tcr-0W3GE1R70W","trusted":true},
              {"id":"tcr-144GJ28914","trusted":true,"lastSeenMs":"not a number"}
            ]}
            """.utf8)
        let read = try PeerListDocument.decodeLivePeers(json)
        XCTAssertTrue(read.supported)
        XCTAssertEqual(read.peers.map(\.id), ["tcr-0W3GE1R70W"])
        XCTAssertEqual(read.unreadable, 1)
    }

    /// An older server answering `{"supported": false}` is a state, and the
    /// caller must be able to tell it from an empty mesh.
    func testALiveReadCanSayItIsUnsupported() throws {
        let read = try PeerListDocument.decodeLivePeers(Data(#"{"supported": false}"#.utf8))
        XCTAssertFalse(read.supported)
        XCTAssertEqual(read.peers, [])
    }

    /// An unknown kind is drawn with its own word rather than silently as a
    /// direct path: mislabelling a relayed path as direct is a claim about
    /// where the bytes went.
    func testAnUnknownKindIsDrawnWithItsOwnWord() {
        let line = PeerFormat.pathLine(
            PeerListDocument.PeerPath(endpoint: "127.0.0.1:7749", kind: .unknown("relay")))
        XCTAssertEqual(line, "relay 127.0.0.1:7749 · not measured")
    }
}
