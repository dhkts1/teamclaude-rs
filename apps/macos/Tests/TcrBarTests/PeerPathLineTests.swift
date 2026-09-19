import XCTest

@testable import TcrBarCore

/// The path sub-line's three states.
final class PeerPathLineTests: XCTestCase {

    private func path(
        endpoint: String = "192.168.1.24:7749",
        kind: PeerListDocument.PeerPath.Kind = .direct,
        rttMs: Double? = nil, lossPct: Double? = nil
    ) -> PeerListDocument.PeerPath {
        .init(endpoint: endpoint, kind: kind, rttMs: rttMs, lossPct: lossPct)
    }

    /// 4a: direct and healthy reads as ordinary text. A measured zero loss is
    /// a finding and says `no loss`; it is not the same as nothing measured.
    func testDirectAndHealthyIsPlain() {
        let lines = PeerFormat.pathLines([path(rttMs: 14, lossPct: 0)])
        XCTAssertEqual(lines.count, 1)
        XCTAssertEqual(lines[0].tone, .plain)
        XCTAssertEqual(lines[0].text, "tried first · 192.168.1.24:7749 · 14 ms · no loss")
    }

    /// 4b: a forwarded path is amber, and it NAMES the forwarder rather than
    /// printing the peer id the wire carries.
    func testAForwardedPathIsAmberAndNamesTheForwarder() {
        let lines = PeerFormat.pathLines(
            [path(endpoint: "tcr-92hbq5t7yv", kind: .via, rttMs: 96, lossPct: 0.06)],
            names: ["tcr-92hbq5t7yv": "loft-mini"])
        XCTAssertEqual(lines[0].tone, .warn)
        XCTAssertEqual(lines[0].text, "tried first · via loft-mini · 96 ms · 6% lost")
    }

    /// A forwarder this Mac has no name for keeps its id. Inventing a name
    /// would be worse than an identifier an operator can look up.
    func testAnUnnamedForwarderKeepsItsId() {
        let lines = PeerFormat.pathLines(
            [path(endpoint: "tcr-92hbq5t7yv", kind: .via, rttMs: 96, lossPct: 0)])
        XCTAssertEqual(lines[0].text, "tried first · via tcr-92hbq5t7yv · 96 ms · no loss")
    }

    /// The 3 per cent line below, checked on both sides of it rather
    /// than claimed. Under it a direct path stays ordinary.
    func testLossCrossesIntoAmberAtThreePerCent() {
        XCTAssertEqual(PeerFormat.pathLines([path(rttMs: 14, lossPct: 0.02)])[0].tone, .plain)
        XCTAssertEqual(PeerFormat.pathLines([path(rttMs: 14, lossPct: 0.03)])[0].tone, .warn)
        XCTAssertEqual(PeerFormat.pathLines([path(rttMs: 14, lossPct: 0.20)])[0].tone, .warn)
    }

    /// A trusted Mac with no path at all
    /// now says so. Before this it drew nothing, so "no way to reach it" and "this
    /// build did not read the live half" looked identical and the asleep pill
    /// did the work of both.
    func testNoPathsSaysSoRatherThanDrawingNothing() {
        let lines = PeerFormat.pathLines([])
        XCTAssertEqual(lines.count, 1)
        XCTAssertEqual(lines[0].text, "no path right now")
        XCTAssertEqual(lines[0].tone, .absent)
    }

    /// And it never prints a number: no RTT, no loss, no zero standing in for
    /// a measurement nobody took.
    func testTheNoPathLineCarriesNoFigures() {
        let text = PeerFormat.pathLines([])[0].text
        XCTAssertFalse(text.contains("ms"))
        XCTAssertFalse(text.contains("%"))
        XCTAssertFalse(text.contains("0"))
    }

    /// An unmeasured path is still a path: it says what it is, and says the
    /// figures are missing, which is not the same as having no path.
    func testAnUnmeasuredPathIsNotTheNoPathState() {
        let lines = PeerFormat.pathLines([path()])
        XCTAssertEqual(lines[0].text, "tried first · 192.168.1.24:7749 · not measured")
        XCTAssertEqual(lines[0].tone, .plain)
        XCTAssertNotEqual(lines[0].text, "no path right now")
    }

    /// Order is the producer's: the first line is the path a dial tries first.
    func testEveryPathGetsALineInTheOrderItArrived() {
        let lines = PeerFormat.pathLines([
            path(endpoint: "192.168.1.24:7749", rttMs: 14, lossPct: 0),
            path(endpoint: "10.0.1.24:7749"),
        ])
        XCTAssertEqual(lines.count, 2)
        XCTAssertTrue(lines[0].text.hasPrefix("tried first · 192.168.1.24:7749"))
        XCTAssertTrue(lines[1].text.hasPrefix("10.0.1.24:7749"))
    }

    /// Nothing on the wire says which path a connection is actually using, so
    /// the first line says only that it is the one tried first, never `in
    /// use`. A row with a single path still gets the prefix: it is still the
    /// path dialled first, even though it is also the only one.
    func testOnlyTheFirstLineSaysTriedFirst() {
        let lines = PeerFormat.pathLines([
            path(endpoint: "192.168.1.24:7749"),
            path(endpoint: "10.0.1.24:7749"),
        ])
        XCTAssertTrue(lines[0].text.hasPrefix("tried first · "))
        XCTAssertFalse(lines[1].text.hasPrefix("tried first · "))
        XCTAssertFalse(lines[1].text.contains("tried first"))
    }

    /// The no-path line is an absence, not a path: it never claims to be
    /// tried first.
    func testTheNoPathLineIsNotLabelledTriedFirst() {
        XCTAssertFalse(PeerFormat.pathLines([])[0].text.contains("tried first"))
    }
}
