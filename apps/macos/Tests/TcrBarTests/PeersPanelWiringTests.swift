import XCTest

@testable import TcrBarCore

/// The findings from a source review of the Peers surfaces, as assertions.
///
/// Every one of them is about WIRING, a helper that exists and nothing calls,
/// a button that runs a verb with its argument missing, a destructive control
/// with no confirm in front of it, and each was invisible to the suite for the
/// same reason: `PeersTabV4` and `PeersSettingsPane` are SwiftUI views in the
/// `TcrBar` executable target, which `Package.swift:39-43` does not give the
/// test target, and ViewInspector is not a dependency here. So this reads the
/// source, the technique `FleetViewContextMenuTests` and
/// `FleetStatusTests.testAccountStatusErrorTokenStillExistsInRustSource`
/// already use for exactly that reason.
///
/// A failure here is usually an ANCHOR that moved, not a regression, each
/// message says which. The trade is deliberate: an anchor that needs updating
/// costs a minute, and a helper nobody calls cost a whole review pass.
///
/// # What is NOT here any more
///
/// A grep is the fallback, not the technique. Everything that could be a value
/// instead was moved into `TcrBarCore`, where the test target can build it and
/// drive it: the argv of every verb and the join key's stdin are
/// `PeerCommandTests`, and the row-shape-to-height step the tab frames its
/// list with is `PeerMeterTests`. What is left below is the part that needs a
/// SwiftUI view to observe, which button opens which sheet, which sheet reads
/// which command's output, plus one check that a value in `TcrBarCore` and an
/// enum in the executable still agree, which is the boundary no linkable test
/// can cross.
final class PeersPanelWiringTests: XCTestCase {

    // MARK: - Height

    /// The finding: `PeerPanelHeight` had ZERO app-side callers. Every figure
    /// in `PeerSectionHeightTests` was arithmetic about a type the running
    /// panel never consulted, so the tab drew at whatever height its content
    /// wanted and the 520 pt cap was a number in a test.
    func testTheRunningTabFramesItsListFromPeerPanelHeight() throws {
        let tab = try source("apps/macos/Sources/TcrBar/PanelV4/PeersTabV4.swift")
        XCTAssertTrue(
            tab.contains("PeerPanelHeight.listViewportHeight("),
            "PeersTabV4 no longer asks PeerPanelHeight how tall the peer list is, "
                + "the cap is back to being a number only a test knows about")
        XCTAssertTrue(
            tab.contains(".frame(height: peerListHeight)"),
            "the peer list's ScrollView is not framed to peerListHeight, so the height "
                + "PeerPanelHeight computes is not the height anything draws")
    }

    /// And it must stay derived from the SNAPSHOT. A `GeometryReader` feeding
    /// the tab's own height back into its frame is one half of the
    /// layout-cycle abort `ffe8a86` fixed (`FleetView.swift:1070-1077` states
    /// the rule for every tab).
    func testThePeerListHeightIsDerivedFromTheSnapshotNotFromAMeasurement() throws {
        let tab = try source("apps/macos/Sources/TcrBar/PanelV4/PeersTabV4.swift")
        let body = try slice(
            tab, from: "private var peerListHeight: CGFloat {", to: "// MARK: Collapsed")
        XCTAssertTrue(
            body.contains("snapshot.rowShapes"),
            "peerListHeight stopped reading the snapshot's row shapes")
        XCTAssertFalse(
            body.contains("GeometryReader"),
            "peerListHeight measures what was drawn and frames the drawing to the result, "
                + "which is the content-dependent tab height FleetView.swift:1070-1077 forbids")
    }

    // MARK: - Show join key

    /// The finding: the button fired `tcr peer invite` and threw away its
    /// stdout, which IS the join key. Nothing appeared, on either screen.
    func testShowJoinKeyReadsTheCommandsOutput() throws {
        let pane = try source("apps/macos/Sources/TcrBar/PeersSettingsView.swift")
        XCTAssertTrue(
            pane.contains("controller.capture(PeerCommand.invite)"),
            "Show join key is not reading the invite's stdout any more, a fire-and-forget "
                + "run(_:) discards the key, which is the whole output of the command")
        XCTAssertTrue(
            pane.contains("NSPasteboard.general.setString(key, forType: .string)"),
            "the join-key sheet has no Copy: the key is a string somebody has to paste on "
                + "another Mac, and it is not selectable from a screenshot")
    }

    /// Three states, so a slow or failed invite is never drawn as an empty
    /// key. `PeerCapture` has no "ran and printed nothing" case for the same
    /// reason.
    func testTheJoinKeySheetDrawsAFailureRatherThanAnEmptyKey() throws {
        let pane = try source("apps/macos/Sources/TcrBar/PeersSettingsView.swift")
        let sheet = try slice(
            pane, from: "private var joinKeySheet: some View {",
            to: "private var pasteKeySheet: some View {")
        XCTAssertTrue(
            sheet.contains("case .failed(let message):"),
            "the join-key sheet no longer renders tcr's own failure message, so a refused "
                + "invite closes or waits forever and looks like a working button")
    }

    // MARK: - Paste a key

    /// The finding: the button ran a bare `tcr peer join`, no key on the end
    /// of it, so the only path for a Mac with no screen could not work at
    /// all.
    ///
    /// Where the key GOES is `PeerCommandTests` now: it is fed to
    /// `tcr peer join --stdin` on stdin and never appears in argv (item 1).
    /// This is the half that needs the view: the field exists, its contents
    /// are what the sheet hands over, and an empty field cannot be submitted.
    func testPasteAKeyPassesTheTypedKeyToTheVerb() throws {
        let pane = try source("apps/macos/Sources/TcrBar/PeersSettingsView.swift")
        XCTAssertTrue(
            pane.contains("PeerCommand.join(key: trimmedPastedKey)"),
            "Paste a key is not passing the field's contents to `tcr peer join` any more")
        XCTAssertTrue(
            pane.contains("TextField(\"Join key\", text: $pastedKey)"),
            "the paste sheet has no text field, so there is nothing for the key to arrive in")
        XCTAssertTrue(
            pane.contains(".disabled(trimmedPastedKey.isEmpty)"),
            "Join is pressable with an empty field, which runs `tcr peer join` with nothing "
                + "on its stdin, a command that waits on a pipe and looks like a hang")
    }

    /// The key must not reach the subprocess as an argument, and there must be
    /// no argv form left to reach for: `PeerController` runs a secret-carrying
    /// verb through ``PeerSecretInvocation``, whose only factory writes both
    /// halves (`PeerCommandTests` asserts the halves themselves).
    func testTheJoinKeyGoesToStdinRatherThanArgv() throws {
        let tab = try source("apps/macos/Sources/TcrBar/PanelV4/PeersTabV4.swift")
        XCTAssertTrue(
            tab.contains("func run(_ invocation: PeerSecretInvocation)"),
            "PeerController has no secret-carrying run any more, so the only way to pass a "
                + "join key is argv, where `ps` shows it to every process on this Mac")
        XCTAssertTrue(
            tab.contains("Self.perform(arguments: arguments, stdin: stdin)"),
            "the run path stopped forwarding stdin: `tcr peer join --stdin` then waits on a "
                + "pipe nobody writes to")

        let core = try source("apps/macos/Sources/TcrBarCore/PeerCommand.swift")
        XCTAssertFalse(
            core.contains(#"["peer", "join", key]"#),
            "the argv form of join is back, one press away from putting the key in `ps`")
    }

    // MARK: - Regenerate, and Forget

    /// The finding: the pane's own sentence said this control "asks first" and
    /// it asked nothing, one press re-minted the identity and evicted every
    /// Mac that had pinned it.
    func testRegenerateAsksFirstAndPassesYesToTheCli() throws {
        let pane = try source("apps/macos/Sources/TcrBar/PeersSettingsView.swift")
        XCTAssertTrue(
            pane.contains("Button(\"Regenerate…\", role: .destructive) { confirmingRegenerate = true }"),
            "Regenerate… runs something directly again instead of opening its confirm")
        XCTAssertTrue(
            pane.contains("isPresented: $confirmingRegenerate"),
            "nothing presents the regenerate confirmation, so the flag is set and no dialog "
                + "appears, the button silently does nothing at all")
        XCTAssertTrue(
            pane.contains("controller.run(PeerCommand.regenerate)"),
            "the confirm's destructive button no longer runs the regenerate verb")
        // `--yes` itself is `PeerCommandTests`: it is a value now, not a line
        // of source in a view.
    }

    /// The same sentence, one section down: "Forget asks once" was prose with
    /// no dialog behind it either.
    func testForgetAsksFirst() throws {
        let pane = try source("apps/macos/Sources/TcrBar/PeersSettingsView.swift")
        XCTAssertTrue(
            pane.contains("Button(\"Forget…\", role: .destructive) { forgetting = row }"),
            "Forget… runs the verb directly again, and the section's own last paragraph "
                + "still promises that it asks once")
        XCTAssertTrue(
            pane.contains("isPresented: forgettingIsPresented"),
            "nothing presents the forget confirmation")
        XCTAssertTrue(
            pane.contains("controller.run(PeerCommand.forget(peer: row.id))"),
            "the forget confirm no longer runs the forget verb")
    }

    /// The pane must not claim to be the only control that asks, now that two
    /// of them do. A true sentence is the deliverable here, not the dialog.
    func testThePaneDoesNotClaimToBeTheOnlyControlThatAsks() throws {
        let pane = try source("apps/macos/Sources/TcrBar/PeersSettingsView.swift")
        XCTAssertFalse(
            pane.contains("the only control on the pane that asks first"),
            "the pane says Regenerate is the only control that asks first, and Forget asks "
                + "too, one of the two sentences is wrong")
    }

    // MARK: - Open on: peers (item 2)

    /// The two executable-side sites that turn ``DefaultTabPreference``'s
    /// stored string back into a `PanelTab`. Both listed the names by hand and
    /// both were short one: the "Open on" picker offered three tags, so
    /// `peers` could not be selected, and `MenuBarShell.initialTab(from:)`
    /// mapped everything it did not recognise to `.accounts`, so a `peers`
    /// this preference ACCEPTS opened the Accounts tab anyway.
    ///
    /// Expectations derived from `DefaultTabPreference.validTabs` rather than
    /// written out again here: a fifth tab must not pass this test by being
    /// missing from a literal list in a test file.
    ///
    /// A grep, and this is the case item 4 leaves as one: `PanelTab` is in the
    /// executable target, `validTabs` is in `TcrBarCore`, and the boundary
    /// between them is exactly what drifted. No test that links one can see
    /// the other.
    func testEveryStorableTabIsOfferedAndOpenable() throws {
        let fleet = try source("apps/macos/Sources/TcrBar/FleetView.swift")
        let panes = try source("apps/macos/Sources/TcrBar/SettingsPanes.swift")
        let shell = try source("apps/macos/Sources/TcrBar/MenuBarShell.swift")

        XCTAssertTrue(
            fleet.contains("enum PanelTab: String, Equatable, CaseIterable"),
            "PanelTab lost its String raw value, the picker's tags and the shell's lookup "
                + "are then two hand-written copies of DefaultTabPreference.validTabs again")
        let cases = try slice(
            fleet, from: "enum PanelTab: String, Equatable, CaseIterable {",
            to: "var title: String")
        for name in DefaultTabPreference.validTabs.sorted() {
            XCTAssertTrue(
                cases.contains(name),
                "PanelTab declares no case \"\(name)\", which DefaultTabPreference stores: "
                    + "the raw value the picker tags would then be one the panel cannot open "
                    + "on. PanelTab's cases are \(cases.trimmingCharacters(in: .whitespacesAndNewlines))")
        }

        XCTAssertTrue(
            panes.contains("ForEach(PanelTab.allCases, id: \\.self) { tab in"),
            "the Open on picker writes its tags by hand again, that list offered three of "
                + "the four tabs, and peers could only be stored by editing UserDefaults")
        XCTAssertTrue(
            panes.contains("Text(tab.title).tag(tab.rawValue)"),
            "the picker's tag is no longer the tab's raw value, so what it stores and what "
                + "MenuBarShell reads back are two different vocabularies")
        XCTAssertTrue(
            shell.contains("PanelTab(rawValue: preference.tab) ?? .accounts"),
            "initialTab(from:) is back to a switch over some of the names: the one it omits "
                + "is stored, accepted, and then opens the Accounts tab")
    }

    // MARK: - One fact, in the one place that owns it

    /// The Mac count belongs to the footer, and the Find card's subtitle is
    /// the only line on the tab that could say what finding DOES.
    ///
    /// The count was printed twice in one scroll, about fifteen points apart:
    /// `Looking. 2 Macs found, 2 trusted.` in this subtitle and `2 Macs
    /// found, 2 trusted` in the footer under the list. Spending the one
    /// describing line on a number that is already below is what this closes.
    func testTheFindCardSubtitleDescribesFindingRatherThanCountingMacs() throws {
        let tab = try source("apps/macos/Sources/TcrBar/PanelV4/PeersTabV4.swift")
        let card = try slice(
            tab, from: "private var findCard: some View {", to: "private var shareCard")
        XCTAssertFalse(
            card.contains("snapshot.countLine"),
            "the Find card prints the Mac count again, a few points above the footer that "
                + "owns it")
        XCTAssertTrue(
            card.contains("Looking. Other Macs running tcr appear below by themselves."),
            "the Looking arm no longer says what finding does, which is the one thing this "
                + "line is for")
    }

    /// A capability and an act get different words.
    ///
    /// The pill means "is willing to relay" and it read as "is relaying now",
    /// on a row whose own path line said its traffic went through somebody
    /// else. The pill states the capability; the path line under it states the
    /// act. And the word is written ONCE, because the pill and the sentence
    /// behind it are the same string in two places.
    func testTheCarryPillNamesTheCapabilityAndIsWrittenOnce() throws {
        let tab = try source("apps/macos/Sources/TcrBar/PanelV4/PeersTabV4.swift")
        XCTAssertTrue(
            tab.contains("static let carryPillText = \"can carry\""),
            "the carry pill's word is not the capability, or is no longer written in one "
                + "place for the pill and its help to share")
        XCTAssertFalse(
            tab.contains("(\"carries\", .info)"),
            "the pill says carries again, which reads as an act on a row whose path line "
                + "says the bytes go through a third Mac")
        XCTAssertFalse(
            tab.contains("case \"carries\": return"),
            "the pill help is keyed on the old word, so the pill an operator hovers has no "
                + "sentence behind it at all")
    }

    /// Neither Trust control wears a checkmark.
    ///
    /// A checkmark is the universal "already done". It was drawn on the found
    /// row's button, whose own subtitle two lines below reads `not trusted`,
    /// and on the sheet's Trust button while that button was disabled. The
    /// word is the control.
    func testNoTrustControlDrawsACheckmark() throws {
        let tab = try source("apps/macos/Sources/TcrBar/PanelV4/PeersTabV4.swift")
        XCTAssertFalse(
            tab.contains("systemImage: \"checkmark\""),
            "a Trust control carries a checkmark again, which reads as done on a row that "
                + "says not trusted and on a button that cannot be pressed yet")
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

    /// The text between two anchors, so an assertion cannot match an
    /// identical line somewhere else in a 2000-line view.
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
