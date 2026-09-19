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

    /// Three absences, three answers, and the CALLER picks.
    ///
    /// One sentence covered two different facts: this Mac looked and found no
    /// way there, and the live half of the read never answered at all. They
    /// are not the same thing to somebody deciding whether to go and look at
    /// the other Mac. The third case is a row with work visibly in flight,
    /// where the traffic is the proof a path exists and the line would
    /// contradict the sentence two lines above it.
    func testTheThreeAbsencesAreThreeDifferentAnswers() {
        XCTAssertEqual(
            PeerFormat.pathLines([], absence: .measured)[0].text, "no path right now")
        XCTAssertEqual(
            PeerFormat.pathLines([], absence: .notReported)[0].text, "path not reported")
        XCTAssertEqual(
            PeerFormat.pathLines([], absence: .notReported)[0].tone, .absent,
            "an unread half is the absence tone, not a warning: nothing is wrong")
        XCTAssertTrue(
            PeerFormat.pathLines([], absence: .silent).isEmpty,
            "a row with traffic on it still prints an absent path under the proof it has one")
        // An absence never overrides real paths: whichever the caller picked,
        // a row with paths draws them.
        XCTAssertEqual(PeerFormat.pathLines([path()], absence: .silent).count, 1)
        XCTAssertEqual(PeerFormat.pathLines([path()], absence: .notReported).count, 1)
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

    /// One of the three absences is a thing a person can answer, and the line
    /// carries that as a FACT rather than leaving it to be read out of the
    /// words.
    ///
    /// A Mac this one has pinned, that it looked for and could not find, is
    /// the whole state the link exists for: both ends moved and neither can
    /// dial the other. The other two absences are not that. `path not
    /// reported` says nothing looked, so there is nothing yet to answer, and
    /// the same measured absence on a Mac this one has not pinned has no key
    /// to seal anything under.
    func testOnlyAMeasuredAbsenceOnATrustedRowIsAControl() {
        let trusted = PeerFormat.pathLines([], absence: .measured, answerable: true)
        XCTAssertEqual(trusted.count, 1)
        XCTAssertTrue(
            trusted[0].actionable,
            "a trusted Mac this one measured no way to reach draws a plain readout, which is "
                + "the one line on the tab that has an answer behind it")
        XCTAssertFalse(
            PeerFormat.pathLines([], absence: .measured)[0].actionable,
            "an untrusted row offers the act, and there is no shared secret to seal a link "
                + "under on a Mac this one has never pinned")
        XCTAssertFalse(
            PeerFormat.pathLines([], absence: .notReported, answerable: true)[0].actionable,
            "an unread live half offers a remedy for a reading this build never took")
        XCTAssertTrue(
            PeerFormat.pathLines([], absence: .silent, answerable: true).isEmpty,
            "the silent absence draws a line at all, and it would be a pressable one")
        XCTAssertFalse(
            PeerFormat.pathLines([path(rttMs: 14, lossPct: 0)], answerable: true)[0].actionable,
            "a row with a path draws its path line as a control")
    }

    /// And the actionable line says the act, in the row's own words, beside
    /// the problem it answers. The words are the tab's, not the view's: a
    /// control whose label is spelled at the call site is a second place the
    /// wording lives.
    func testTheActionableLineNamesTheActBesideTheProblem() {
        let line = PeerFormat.pathLines([], absence: .measured, answerable: true)[0]
        XCTAssertEqual(line.text, "no path right now · send it a link")
        XCTAssertEqual(
            line.tone, .absent,
            "the line changed tone when it became a control, and it is still an absence")
        XCTAssertEqual(
            PeerFormat.pathLines([], absence: .measured)[0].text, "no path right now",
            "a row with no act to offer says the act anyway")
    }
}
