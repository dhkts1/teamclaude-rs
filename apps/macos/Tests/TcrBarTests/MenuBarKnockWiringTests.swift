import XCTest

@testable import TcrBarCore

/// The menu bar's half of "a knock that nobody sees", read off the source.
///
/// `MenuBarShell` is in the `TcrBar` executable target, which this test target
/// does not link (`Package.swift` gives it `TcrBarCore` alone), so the claims
/// about the status item are made against its text — the same arrangement
/// `PeersPanelWiringTests` uses for the panel. What can be a value is a value:
/// the sentence, the spoken description and the reader's decoding are tested
/// directly in this target.
final class MenuBarKnockWiringTests: XCTestCase {

    // MARK: - The reader is owned by the shell and started at launch

    /// It must outlive the panel. The Peers tab's own `PeerController` starts
    /// on `onAppear` and stops on `onDisappear`, which is exactly the case
    /// this feature is for: the panel is shut.
    func testTheShellOwnsTheKnockReaderAndTheAppStartsIt() throws {
        let shell = try source("apps/macos/Sources/TcrBar/MenuBarShell.swift")
        XCTAssertTrue(
            shell.contains("let knocks: KnockReader"),
            "the shell no longer owns a knock reader, so nothing reads pending knocks with "
                + "the panel closed")
        let app = try source("apps/macos/Sources/TcrBar/TcrBarApp.swift")
        XCTAssertTrue(
            app.contains("shell.knocks.start()"),
            "the reader is never started, so the bar mark can only ever be absent")
        XCTAssertTrue(
            shell.contains(".combineLatest(self.knocks.$knocks)"),
            "the mark is not recomposed when a knock arrives, so it would wait for an "
                + "account figure to change")
    }

    // MARK: - The segment is drawn whether or not the counts are

    /// The title is built in two independent halves, knock first.
    ///
    /// `updateMark` used to write `button.title = ""` in the `else` of the
    /// counts preference, which would erase a knock segment on every Mac with
    /// counts off — the default.
    func testTheKnockSegmentIsDrawnAheadOfAndIndependentlyOfTheCounts() throws {
        let shell = try source("apps/macos/Sources/TcrBar/MenuBarShell.swift")
        let title = try slice(
            shell, from: "static func markTitle(", to: "private func updateMark(")
        let segment = try XCTUnwrap(title.range(of: "knockAttributedSegment(count: knocks)"))
        let counts = try XCTUnwrap(title.range(of: "Self.countsAttributedTitle("))
        XCTAssertTrue(
            segment.lowerBound < counts.lowerBound,
            "the counts are composed before the knock, so the order on the bar is not "
                + "\"what wants an answer, then what the fleet is doing\"")
        XCTAssertTrue(
            title.contains("guard showCounts, let label = state.countsLabel else { return title }"),
            "the counts preference gates more than the counts again")
        XCTAssertFalse(
            shell.contains("button.title = \"\"\n            }"),
            "the empty-title else branch is back, and it writes over the knock segment")
    }

    /// Colour is never the only channel: the pointer route and the spoken one
    /// both carry the sentence.
    func testTheTooltipAndTheMarkBothCarryTheSentence() throws {
        let shell = try source("apps/macos/Sources/TcrBar/MenuBarShell.swift")
        XCTAssertTrue(
            shell.contains(
                "Self.toolTip(\n            state: state, awake: isOn, "
                    + "showRunningTools: showRunningTools, knocks: knocks)"),
            "the tooltip stopped being told about knocks, so the amber glyph has no words")
        XCTAssertTrue(
            shell.contains("tint: Self.cupTint(for: state, awake: isOn),\n            knocks: knocks"),
            "the mark's accessibility description stopped being told about knocks, so "
                + "VoiceOver says nothing about a Mac waiting on an answer")
    }

    /// The bar says a request exists and how many. It never says WHO: a
    /// proposed name is a string a stranger's Mac chose, and there is no room
    /// for the address that would let an operator check it.
    func testTheBarNamesNobody() throws {
        let shell = try source("apps/macos/Sources/TcrBar/MenuBarShell.swift")
        let segment = try slice(
            shell, from: "static func knockAttributedSegment(", to: "static func markTitle(")
        for naming in ["proposedName", "knock.addr", "knockNameLine"] {
            XCTAssertFalse(
                segment.contains(naming),
                "the menu bar segment reaches for \(naming): the bar says how many, never who")
        }
        XCTAssertTrue(
            segment.contains("person.fill.questionmark"),
            "the glyph is no longer the one the design chose")
        XCTAssertTrue(
            segment.contains("Tok.nearNSColor"),
            "the segment is drawn in some colour other than the tab's own amber")
    }

    /// The tenth scene and its siblings: the glyph at 13 pt cannot be judged
    /// in prose, and it has to be seen with the counts on and off, in both
    /// appearances.
    func testTheMarkHarnessRendersTheKnockScenes() throws {
        let harness = try source("apps/macos/Sources/TcrBar/RenderMark.swift")
        for scene in [
            "10-dark-knock-one", "11-dark-knock-two-counts-on", "12-light-knock-one",
            "13-light-knock-two-counts-on",
        ] {
            XCTAssertTrue(
                harness.contains("\"\(scene)\""),
                "\(scene) has no fixture, so the mark it draws is unreviewable")
        }
        XCTAssertTrue(
            harness.contains("MenuBarShell.markTitle("),
            "the harness composes its own title again, so a fixture and the live bar can "
                + "draw the knock segment two ways")
    }

    // MARK: - The words

    /// One Mac and several, and nothing at all at zero.
    func testTheBarSentenceCountsAndSaysWhereToAnswer() {
        XCTAssertEqual(
            PeerAdmission.knockBarSentence(count: 1),
            "1 Mac is asking to connect. Open the Peers tab to answer it.")
        XCTAssertEqual(
            PeerAdmission.knockBarSentence(count: 2),
            "2 Macs are asking to connect. Open the Peers tab to answer them.")
        XCTAssertNil(
            PeerAdmission.knockBarSentence(count: 0),
            "a Mac that has never met another one would be told 0 Macs are asking")
        XCTAssertNil(PeerAdmission.knockBarSentence(count: -1))
    }

    /// VoiceOver hears the request FIRST, and at zero knocks the description
    /// is byte-identical to what it has always been.
    func testTheSpokenDescriptionLeadsWithTheRequestAndIsUnchangedWithout() {
        XCTAssertEqual(
            MenuBarMark.accessibilityDescription(awake: false, knocks: 0), "tcr fleet capacity")
        XCTAssertEqual(
            MenuBarMark.accessibilityDescription(awake: false),
            MenuBarMark.accessibilityDescription(awake: false, knocks: 0),
            "the default argument changed what an existing caller hears")
        let asking = MenuBarMark.accessibilityDescription(awake: true, knocks: 2)
        XCTAssertTrue(asking.hasPrefix("2 Macs are asking to connect."), asking)
        XCTAssertTrue(
            asking.contains("tcr fleet capacity"),
            "the capacity sentence was replaced rather than led: \(asking)")
    }

    // MARK: - Helpers

    private func repoRoot() -> URL {
        URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent()  // this file -> TcrBarTests
            .deletingLastPathComponent()  // TcrBarTests -> Tests
            .deletingLastPathComponent()  // Tests -> apps/macos
            .deletingLastPathComponent()  // apps/macos -> apps
            .deletingLastPathComponent()  // apps -> repo root
    }

    private func source(_ relativePath: String) throws -> String {
        try String(
            contentsOf: repoRoot().appendingPathComponent(relativePath), encoding: .utf8)
    }

    /// The text between two anchors, so an assertion cannot match an identical
    /// line somewhere else in the file.
    private func slice(_ contents: String, from: String, to: String) throws -> String {
        guard let start = contents.range(of: from),
            let end = contents.range(of: to, range: start.upperBound..<contents.endIndex)
        else {
            XCTFail("an anchor has moved: \"\(from)\" … \"\(to)\"")
            return ""
        }
        return String(contents[start.upperBound..<end.lowerBound])
    }
}
