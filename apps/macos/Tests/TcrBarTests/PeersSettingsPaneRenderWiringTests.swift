import XCTest

@testable import TcrBarCore

/// Three defects in the Settings > Peers pane, as assertions about
/// the source that drew them.
///
/// # Why these read the source
///
/// Every one of them is about a SwiftUI view's structure, which modifier a
/// field carries, what a section head holds, which origin the render harness
/// scrolls to, and the test target links `TcrBarCore` alone: it cannot build
/// the pane, and `PeersSettingsPane` is not visible to it. The instrument for
/// the drawn result is `--render-settings` plus a person's eye
/// (`/tmp/lan-p2p/panel5/final/peers-{dark,light}.png`); these gates are what
/// stops the source silently going back, and `PeerPaneLayoutTests` holds the
/// numbers.
///
/// Same shape as `PeersPanelViewWiringTests`, including its anchor-slicing, so one
/// assertion cannot match an identical line elsewhere in a 1,100-line view.
final class PeersSettingsPaneRenderWiringTests: XCTestCase {

    // MARK: - Item 1: the first section head is not under the title bar

    /// The harness scrolls FROM the clip view's resting origin.
    ///
    /// The defect: `scroll(to: NSPoint(x: 0, y: offset))` with `offset == 0`
    /// discards a `.fullSizeContentView` window's 52 pt top inset and drags
    /// the whole document under the title bar, which is exactly what the
    /// reviewed capture showed, while the log said `scrolled to 0 pt`.
    func testTheHarnessScrollsFromTheRestingOrigin() throws {
        let harness = try source("apps/macos/Sources/TcrBar/RenderSettings.swift")
        XCTAssertTrue(
            harness.contains("let restingY = scroll.contentView.bounds.origin.y"),
            "the harness no longer reads where the pane RESTS, so it cannot scroll relative "
                + "to it")
        XCTAssertTrue(
            harness.contains(
                "RenderScrollTarget.clipOrigin(restingY: restingY, offset: offset)"),
            "the scroll target is not the resting origin plus the offset any more, a bare "
                + "offset throws the toolbar inset away and hides the first section head")
    }

    /// The viewport figure is what an operator SEES, not the clip view's
    /// bounds.
    ///
    /// `contentView.bounds.height` is the frame plus the inset (592 for a
    /// 540 pt clip under a 52 pt toolbar), so using it overstated the visible
    /// height by 104 pt: the printed "document ≤ viewport" read as a fit while
    /// the pane scrolled, and every capture under-scrolled by the same amount.
    func testTheViewportIsTheVisibleHeightNotTheClipBounds() throws {
        let harness = try source("apps/macos/Sources/TcrBar/RenderSettings.swift")
        XCTAssertTrue(
            harness.contains("let viewportHeight = scroll.contentView.frame.height + restingY"),
            "the viewport is not the clip's frame less the toolbar inset any more, so the "
                + "line this harness prints is no longer a claim about what fits")
        XCTAssertFalse(
            harness.contains("let viewportHeight = scroll.contentView.bounds.height"),
            "`bounds.height` is back: it is the frame PLUS the inset, and it makes every "
                + "pane look like it fits")
    }

    /// Peers is captured in the window `SettingsWindowController` opens, from
    /// that controller's own constant.
    ///
    /// A grown window made the capture unfalsifiable: in a viewport taller
    /// than any document, "document ≤ viewport" is true by construction.
    func testThePeersCaptureUsesTheShippedWindowSize() throws {
        let harness = try source("apps/macos/Sources/TcrBar/RenderSettings.swift")
        XCTAssertTrue(
            harness.contains("return SettingsWindowController.shippedContentSize"),
            "the Peers capture is sized by something other than the shipped window again")
        let controller = try source("apps/macos/Sources/TcrBar/SettingsWindowController.swift")
        XCTAssertTrue(
            controller.contains(
                "nonisolated static let shippedContentSize = NSSize(width: 660, height: 540)"),
            "the window's content size is not a named constant any more, so the harness and "
                + "the window can drift apart")
        XCTAssertTrue(
            controller.contains("size: Self.shippedContentSize"),
            "the window builds itself from a literal again rather than from the constant the "
                + "harness reads")
    }

    // MARK: - Item 2: the height budget

    /// The pane takes its metrics from `TcrBarCore` rather than holding a
    /// second copy of the twelve measured numbers.
    func testThePaneHoldsNoSecondCopyOfTheDrawnMetrics() throws {
        let pane = try source("apps/macos/Sources/TcrBar/PeersSettingsView.swift")
        XCTAssertTrue(
            pane.contains("static let shippedMetrics = PeerPaneLayout.drawnMetrics"),
            "the pane writes its own metric figures again, they are measured off a capture "
                + "and the gate reads them from TcrBarCore, so a second copy is the one that "
                + "drifts")
        XCTAssertFalse(
            pane.contains("sectionHeadHeight: 21"),
            "a metrics literal is back in the view")
    }

    /// No `applied live` / `plaintext` pill in a section head, and no head
    /// helper that could put one there.
    func testNoBadgePillsInTheSectionHeads() throws {
        let pane = try source("apps/macos/Sources/TcrBar/PeersSettingsView.swift")
        XCTAssertFalse(
            pane.contains("PeersSectionHeader("),
            "the badge-carrying section head is back; the pills came off the heads "
                + "and the head is the title plus one sentence")
        let head = try slice(
            pane, from: "    private func peersSectionHead(", to: "    private func peersRow(")
        XCTAssertFalse(
            head.contains("PeersTag("),
            "the timing badge is in the section head again, bracketing the one word the "
                + "operator navigates by")
        XCTAssertFalse(
            pane.contains("struct PeersPlaintextTag"),
            "the plaintext PILL is back. The plaintext meaning is the Sharing head's own "
                + "sentence, in the hue reserved for a request another Mac reads, the word "
                + "carries it and the colour is the second channel")
        XCTAssertTrue(
            head.contains("plaintext ? Tok.unknown : Tok.inkFaint"),
            "the Sharing head's sentence has lost the plaintext hue")
    }

    /// The Defaults row is one line: the short readout, and a promise that a
    /// longer one truncates rather than growing the pane.
    func testTheDefaultsRowIsOneLine() throws {
        let pane = try source("apps/macos/Sources/TcrBar/PeersSettingsView.swift")
        let sharing = try slice(
            pane, from: "    private var sharing: some View {", to: "// MARK: - 3. Trusted Macs")
        XCTAssertTrue(
            sharing.contains("Text(PeerLease.defaultsLine)"),
            "the Defaults row computes its own readout again; the short spelling is in "
                + "TcrBarCore where its length is gated")
        XCTAssertTrue(
            sharing.contains(".lineLimit(1)"),
            "the Defaults readout may wrap again, and a wrapped value is a 64 pt row where a "
                + "single line is 40")
    }

    /// The knock sentence is drawn ONCE, by the tab. In this pane it reaches a
    /// screen reader as a hint instead of costing a second caption row.
    func testTheKnockRowKeepsItsSentenceForScreenReadersOnly() throws {
        let pane = try source("apps/macos/Sources/TcrBar/PeersSettingsView.swift")
        let row = try slice(
            pane, from: "    private func knockRow(", to: "    // MARK: - 4. Advanced")
        XCTAssertTrue(
            row.contains(".accessibilityHint(PeerAdmission.knockDetail)"),
            "the pairing row no longer carries PeerAdmission's sentence at all, it cost a "
                + "caption row on screen, but dropping it from the accessibility tree too "
                + "would take the decision's own words away from the operator who cannot see "
                + "the buttons")
        XCTAssertFalse(
            row.contains("peersRow(PeerAdmission.knockDetail)"),
            "the sentence is a drawn caption row again, which is 30 pt this pane does not "
                + "have")
        let tab = try source("apps/macos/Sources/TcrBar/PanelV4/PeersTabV4.swift")
        XCTAssertTrue(
            tab.contains("PeerAdmission.knockDetail"),
            "the Peers TAB has stopped drawing the sentence, so now nowhere says what "
                + "Accept does")
    }

    // MARK: - Item 3: Name is the editable field

    /// The name is a bordered `TextField` that commits on return, and the verb
    /// it runs is `tcr peer name <name>`.
    ///
    /// A borderless field in a grouped `Form` renders as right-aligned grey
    /// text and reads as a label, which is what the reviewed capture showed.
    func testTheNameIsAnEditableFieldThatCommitsOnReturn() throws {
        let pane = try source("apps/macos/Sources/TcrBar/PeersSettingsView.swift")
        let section = try slice(
            pane, from: "    private var thisMac: some View {", to: "// MARK: - 2. Sharing")
        XCTAssertTrue(
            section.contains("TextField(\"Name\", text: $editedName"),
            "the name is not a text field any more")
        XCTAssertTrue(
            section.contains(".textFieldStyle(.roundedBorder)"),
            "the name field lost its border, which is what made it read as a label")
        XCTAssertTrue(
            section.contains(".onSubmit { commitName() }"),
            "the field no longer commits on return, and there is no other control that "
                + "writes it")
        let commit = try slice(
            pane, from: "    private func commitName() {", to: "    /// `confirmationDialog(")
        XCTAssertTrue(
            commit.contains("controller.run([\"peer\", \"name\", trimmed])"),
            "committing the name runs something other than tcr peer name")
        XCTAssertTrue(
            commit.contains("trimmed != displayName"),
            "an unchanged name is written again, a write per visit, for nothing")
    }

    /// The field shows the name `tcr` reports, and a poll never overwrites
    /// what somebody is typing.
    func testTheFieldIsFilledFromTheSnapshotButNeverOverTyping() throws {
        let pane = try source("apps/macos/Sources/TcrBar/PeersSettingsView.swift")
        XCTAssertTrue(
            pane.contains(".onChange(of: snapshot.name) { latest in fillNameIfEmpty(latest) }"),
            "the field is no longer filled from what tcr reports, so it shows an empty box "
                + "with a grey prompt, the thing that read as a label")
        let fill = try slice(
            pane, from: "    private func fillNameIfEmpty(", to: "    private func commitName()")
        XCTAssertTrue(
            fill.contains("guard editedName.isEmpty"),
            "the fill no longer checks that the field is empty, so a poll arriving mid-word "
                + "overwrites what the operator typed")
    }

    /// This Mac's id is the Name row's sub-line, and it is not ALSO a row
    /// behind Advanced.
    func testTheIdIsTheNameRowsSubLineAndNowhereElse() throws {
        let pane = try source("apps/macos/Sources/TcrBar/PeersSettingsView.swift")
        let section = try slice(
            pane, from: "    private var thisMac: some View {", to: "// MARK: - 2. Sharing")
        XCTAssertTrue(
            section.contains("Text(nodeId)"),
            "the id is not under the name any more; it is the one read-only fact an "
                + "operator compares against another screen")
        XCTAssertFalse(
            pane.contains("LabeledContent(\"Id\")"),
            "the id is a row behind Advanced again as well as a sub-line, one fact, two "
                + "places, and they are formatted differently")
    }

    // MARK: - Item 4: the internet row's retry button fires something

    /// The button existed with no `onRetry:` argument reaching it, so it drew
    /// and did nothing: the exact "check the surface a user reads" defect
    /// this repo's own rules warn about, since every gate below the render
    /// stays green whether the button is wired or not.
    func testTheInternetRowIsPassedAnOnRetryClosure() throws {
        let pane = try source("apps/macos/Sources/TcrBar/PeersSettingsView.swift")
        XCTAssertTrue(
            pane.contains("onRetry: retryReach"),
            "PeerInternetRow is built with no onRetry: argument, so its button fires "
                + "nothing")
    }

    /// The retry button never re-runs `PeerCommand.internet`: the switch
    /// itself has not moved, only the probe under it.
    func testRetryReachRunsTheProbeAloneNeverTheSwitch() throws {
        let pane = try source("apps/macos/Sources/TcrBar/PeersSettingsView.swift")
        let retry = try slice(
            pane, from: "    private func retryReach() {", to: "    private var announceNameBinding:")
        XCTAssertTrue(
            retry.contains("controller.capture(PeerCommand.reach)"),
            "retryReach no longer runs the reach probe")
        XCTAssertFalse(
            retry.contains("controller.run(PeerCommand.internet"),
            "retryReach writes the switch again; a retry must never move it")
    }

    /// The in-flight flag is threaded into the one place that decides the
    /// row's state, so the button reads Asking… and goes unavailable while a
    /// retry is outstanding.
    func testTheRetryFlagReachesTheStateDecision() throws {
        let pane = try source("apps/macos/Sources/TcrBar/PeersSettingsView.swift")
        XCTAssertTrue(
            pane.contains("@State private var isRetryingReach = false"),
            "the in-flight retry flag is gone, so the row can no longer tell asking from "
                + "retrying")
        let state = try slice(
            pane, from: "    private var internetState: PeerInternetReach {",
            to: "    /// The press: write the setting")
        XCTAssertTrue(
            state.contains("retrying: isRetryingReach"),
            "internetState no longer passes retrying: into PeerInternetReach.state, so a "
                + "retry press reads as an ordinary asking instead of its own case")
    }

    // MARK: - Reading the source

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

    private func slice(_ contents: String, from: String, to: String) throws -> String {
        guard let start = contents.range(of: from) else { throw Anchor.missing(from) }
        let rest = contents[start.upperBound...]
        guard let end = rest.range(of: to) else { throw Anchor.missing(to) }
        return String(rest[..<end.lowerBound])
    }

    private enum Anchor: Error, CustomStringConvertible {
        case missing(String)

        var description: String {
            switch self {
            case .missing(let anchor):
                return "anchor no longer in the source: \(anchor)"
            }
        }
    }
}
