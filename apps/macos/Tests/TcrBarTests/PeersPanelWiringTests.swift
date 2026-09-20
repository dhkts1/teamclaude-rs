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

    /// The finding this pinned originally: the button fired `tcr peer invite`
    /// and threw away its stdout, which IS the join key. Nothing appeared, on
    /// either screen.
    ///
    /// The pairing-link build's phase 3 pointed this button at the same
    /// sheet and classifier the Peers tab's footer uses
    /// (`pairing-link-design.md`, "The subtraction"), so the assertion below
    /// moved from a bespoke `PeerCapture` read to the shared `mintInvite` and
    /// `PeerInviteSheet`, and it is the answer to the same finding: the key
    /// still has to reach a screen, and Copy still has to work.
    func testShowJoinKeyReadsTheCommandsOutput() throws {
        let pane = try source("apps/macos/Sources/TcrBar/PeersSettingsView.swift")
        XCTAssertTrue(
            pane.contains("controller.mintInvite {"),
            "Show join key is not reading the invite's stdout any more, a fire-and-forget "
                + "run(_:) discards the key, which is the whole output of the command")
        let tab = try source("apps/macos/Sources/TcrBar/PanelV4/PeersTabV4.swift")
        let sheet = try slice(
            tab, from: "struct PeerInviteSheet: View {", to: "// MARK: - The link sheet")
        XCTAssertTrue(
            sheet.contains("NSPasteboard.general.setString(key, forType: .string)"),
            "the shared invite sheet has no Copy: the key is a string somebody has to paste "
                + "on another Mac, and it is not selectable from a screenshot")
    }

    /// Three states, so a slow or failed invite is never drawn as an empty
    /// key. Phase 3 pointed Show join key at the same sheet the footer's
    /// Invite… button opens rather than keeping a second Swift copy of this
    /// rule, so the assertion is against the shared sheet now.
    func testTheJoinKeySheetDrawsAFailureRatherThanAnEmptyKey() throws {
        let tab = try source("apps/macos/Sources/TcrBar/PanelV4/PeersTabV4.swift")
        let sheet = try slice(
            tab, from: "struct PeerInviteSheet: View {", to: "// MARK: - The link sheet")
        XCTAssertTrue(
            sheet.contains("case .couldNotRun(let said):"),
            "the shared invite sheet no longer renders tcr's own failure message, so a "
                + "refused invite closes or waits forever and looks like a working button")
        let pane = try source("apps/macos/Sources/TcrBar/PeersSettingsView.swift")
        XCTAssertFalse(
            pane.contains("private var joinKeySheet: some View {"),
            "the pane's own copy of the key sheet is back, which is the second spelling of "
                + "\"works once and expires in ten minutes\" phase 3 removed")
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

    /// No drawing of the mesh until there is a mesh.
    ///
    /// With nothing trusted the card was a paragraph saying there is nothing
    /// to draw, stacked directly above another card saying there is nothing
    /// found: about two fifths of the panel spent on two absences, and the
    /// found card below already says it.
    func testTheMeshCardIsDrawnOnlyWhenAMacIsTrusted() throws {
        let tab = try source("apps/macos/Sources/TcrBar/PanelV4/PeersTabV4.swift")
        let body = try slice(tab, from: "                findCard", to: "ForEach(snapshot.pending)")
        XCTAssertTrue(
            body.contains("if snapshot.trustedCount > 0 {"),
            "the mini mesh is drawn unconditionally again, so an empty drawing sits above "
                + "an empty found card")
        XCTAssertTrue(
            body.contains("MiniMeshCard("),
            "the mesh card is gone from the tab altogether, not just from the empty state")
    }

    /// The carry sentence is said once, above the list, and the rows that do
    /// not deviate from it say nothing.
    ///
    /// It was repeated word for word on every trusted row, seven times in the
    /// seven-Mac state, for a fact that is true of every trusted Mac. A row
    /// keeps a sentence of its own only where it deviates, which is the
    /// gateway row with its own byte figures.
    func testTheCarrySentenceIsSaidOnceAboveTheList() throws {
        let tab = try source("apps/macos/Sources/TcrBar/PanelV4/PeersTabV4.swift")
        XCTAssertTrue(
            tab.contains("static let carrySentence ="),
            "the shared carry sentence is not one string any more")
        let head = try slice(
            tab, from: "private var sectionHead: some View {", to: "private var countLine")
        XCTAssertTrue(
            head.contains("PeersSnapshotBuilder.carrySentence"),
            "the section head no longer carries the sentence, so the fact is said nowhere")
        XCTAssertFalse(
            tab.contains("\"Carries your traffic when this Mac has no route of its own"),
            "the per-row copy of the carry sentence is back, once per trusted row, and it "
                + "still says route where the tab says path")
        let metrics = try slice(
            tab, from: "static var heightMetrics: PeerPanelHeight.Metrics {", to: "private func pillHelp")
        XCTAssertTrue(
            metrics.contains("carrySentenceLines"),
            "the height budget does not charge for the sentence under the section head, and "
                + "growth the budget cannot see comes out of the footer")
    }

    /// The row decides which absence its empty path list means, and a row
    /// with traffic on it prints none.
    ///
    /// `PeerFormat` cannot answer this: only the caller knows whether the
    /// live half answered and whether work is in flight. The row printed
    /// `2 requests are on studio-mac's accounts now` two lines above `no path
    /// right now`, which is one card saying traffic is flowing over a route
    /// it also says does not exist.
    func testTheRowPicksWhichAbsenceItsPathListMeans() throws {
        let tab = try source("apps/macos/Sources/TcrBar/PanelV4/PeersTabV4.swift")
        let builder = try slice(
            tab, from: "private static func row(", to: "private static func pills(")
        XCTAssertTrue(
            builder.contains("let working = (entry.inFlight ?? 0) > 0"),
            "traffic no longer silences the absent path line, so a row can contradict "
                + "itself two lines apart")
        XCTAssertFalse(
            builder.contains("meter.isLiveLease"),
            "a standing lease with nothing on it silences the line again, and that is not "
                + "traffic: it cost a sleeping Mac's row the one true line it had")
        XCTAssertTrue(
            builder.contains("working ? .silent"),
            "a row with work in flight prints an absence again")
        XCTAssertTrue(
            builder.contains("liveAnswered == false ? .notReported : .measured"),
            "a live half that never answered is reported as a measured absence, which is a "
                + "claim this panel did not measure")
        XCTAssertTrue(
            builder.contains("absence: absence"),
            "the decision is made and then not handed to the lines it decides")
    }

    /// The mini mesh is told whether a Mac has a path, rather than guessing it
    /// from the figures.
    ///
    /// `PeerMeshPeer.hasPath` defaults to `true`, so a tile built without it
    /// claims every trusted Mac is reachable. The row already knows the answer
    /// and the card already draws two different pictures for it, a dotted edge
    /// with a plate against a solid one, and nothing carried the fact across
    /// the one line between them.
    ///
    /// Not derived from `rttMs` at the far end either: a path that is known and
    /// has never been probed has no number, and reading that as no path would
    /// put the plate on a Mac this one can reach.
    func testTheMeshIsHandedTheRowsOwnPathFact() throws {
        let tab = try source("apps/macos/Sources/TcrBar/PanelV4/PeersTabV4.swift")
        let mesh = try slice(
            tab, from: "var meshPeers: [PeerMeshPeer] {", to: "var trustedCount: Int")
        XCTAssertTrue(
            mesh.contains("hasPath: row.hasPath"),
            "the mesh is built without the row's path fact, so a Mac with no path reaches the "
                + "card as hasPath: true and is drawn with a solid edge to a Mac nothing can "
                + "reach")
        XCTAssertFalse(
            mesh.contains("hasPath: row.pathRttMs != nil"),
            "the fact is re-derived from a measurement, which draws the no-path plate on a "
                + "Mac whose one endpoint has simply never been probed")
    }

    /// The unsupported card offers the update it asks for, and offers it only
    /// when it can really run one.
    ///
    /// It told an operator to update `tcr` and gave them nothing to press,
    /// while the app ships an update flow the menu bar item runs. The panel
    /// cannot reach the shell's updater by itself, so the act is injected: a
    /// caller that has one hands it over, and with none the control is not
    /// drawn at all rather than wired to a closure that does nothing.
    func testTheUnsupportedCardOffersTheUpdateItAsksFor() throws {
        let tab = try source("apps/macos/Sources/TcrBar/PanelV4/PeersTabV4.swift")
        XCTAssertTrue(
            tab.contains("var onCheckForUpdates: (() -> Void)?"),
            "the update act is not injected any more, so the panel either reaches a global "
                + "or the card is back to naming an act it cannot perform")
        XCTAssertTrue(
            tab.contains("checkForUpdates: onCheckForUpdates"),
            "the unsupported card does not hand its control the act, so pressing it does "
                + "nothing")
        let card = try slice(
            tab, from: "private func collapsed(", to: "/// The refused verb, in `tcr`'s own")
        XCTAssertTrue(
            card.contains("if let checkForUpdates {"),
            "the button is drawn whether or not there is an update check behind it, which is "
                + "a control that lies")
        XCTAssertTrue(
            card.contains("title: \"Check for updates…\""),
            "the control is not the one the card's own sentence asks for")
    }

    /// And something reaches the tab with an update check to hand it.
    ///
    /// The injection above is only half the wiring: with no caller passing one,
    /// `onCheckForUpdates` is `nil` on every panel the app ever draws, the card
    /// draws no button, and the whole arm is a branch nothing takes. The act
    /// comes off the `Updater` `FleetView` already holds, the same one the
    /// header's own update button presses, because two update paths is how the
    /// two start disagreeing about whether a check is already running.
    func testTheRunningPanelHandsThePeersTabTheAppsOwnUpdateCheck() throws {
        let fleet = try source("apps/macos/Sources/TcrBar/FleetView.swift")
        let view = try slice(fleet, from: "struct PeersView: View {", to: "struct Sparkline")
        XCTAssertTrue(
            view.contains("private let onCheckForUpdates: (() -> Void)?"),
            "the peers view does not carry an update check, so nothing between the shell's "
                + "updater and the card can pass one down")
        XCTAssertTrue(
            view.contains("onCheckForUpdates: onCheckForUpdates"),
            "the peers view takes an update check and does not hand it to the tab")
        XCTAssertEqual(
            fleet.components(separatedBy: "onCheckForUpdates: { updater.checkForUpdates() }")
                .count - 1,
            2,
            "both panels that draw the peers tab must hand it the act; one of them offers a "
                + "card that tells an operator to update and gives them nothing to press")
    }

    /// A Mac with no network at all says so, in both cards, and only when the
    /// running tcr reported it.
    ///
    /// It was drawn as "looking", with a card under it explaining that only
    /// Macs on this network can appear, to somebody who is on no network.
    func testTheFindCardHasANoNetworkArmThatAbsenceCannotTrigger() throws {
        let tab = try source("apps/macos/Sources/TcrBar/PanelV4/PeersTabV4.swift")
        XCTAssertTrue(
            tab.contains("private var noNetwork: Bool { snapshot.network == false }"),
            "the no-network arm is not keyed on a REPORTED false, so every tcr that does not "
                + "report interfaces draws a network failure that is nothing of the sort")
        let card = try slice(
            tab, from: "private var findCard: some View {", to: "private var shareCard")
        XCTAssertTrue(
            card.contains(
                "\"No network. This Mac is not on Wi-Fi or Ethernet, so there is \""),
            "the Find card claims it is looking on a Mac that cannot look")
        let empty = try slice(
            tab, from: "private var emptyCard: some View {", to: "private var sectionHead")
        XCTAssertTrue(
            empty.contains("\"No network\"")
                && empty.contains("Join a Wi-Fi network or plug in a cable."),
            "the card under it still explains that only Macs on this network can appear, to "
                + "somebody who is on no network")
    }

    // MARK: - The line that answers itself

    /// The absent line is a control where it can be answered, and a readout
    /// everywhere else, and the row decides which from FACTS.
    ///
    /// The view may not work this out from the words. A sentence compared
    /// against a copy of itself held in a view is the second place one string
    /// has to stay spelled the same, and the one that goes stale is the one
    /// nobody is reading.
    func testTheAbsentPathLineIsAControlOnlyWhereItCanBeAnswered() throws {
        let tab = try source("apps/macos/Sources/TcrBar/PanelV4/PeersTabV4.swift")
        let builder = try slice(
            tab, from: "private static func row(", to: "private static func pills(")
        XCTAssertTrue(
            builder.contains("answerable: entry.id != nil"),
            "the row no longer says whether its absent line can be answered, so either every "
                + "row offers the act or none does, and a row whose wire carried no id offers "
                + "a press that can only come back refused")
        let card = try slice(
            tab, from: "private func peerCard(_ row: PeerRowModel) -> some View {",
            to: "/// Serve the mesh as a page")
        XCTAssertTrue(
            card.contains("if line.actionable {") && card.contains("pathControl(row, line: line)"),
            "the path lines are all drawn as plain text again, so the one line with an act "
                + "behind it is a readout and the feature has no surface at all")
        XCTAssertFalse(
            card.contains("no path right now"),
            "the row decides what this line is by reading its own words, which is a second "
                + "copy of a sentence PeerFormat owns")
    }

    /// The press starts the mint for THAT row's Mac, and the id it hands over
    /// is the row's own.
    ///
    /// A sheet opened from one row and minting for another is a link sealed
    /// for the wrong Mac: it would open against nothing at the far end, and
    /// the refusal there names nobody, so neither person could tell what went
    /// wrong.
    func testTheLinkSheetMintsForTheRowItWasOpenedFrom() throws {
        let tab = try source("apps/macos/Sources/TcrBar/PanelV4/PeersTabV4.swift")
        XCTAssertTrue(
            tab.contains("controller.mintMovedLink(peer: row.id)"),
            "the press no longer mints for the row it came from")
        XCTAssertTrue(
            tab.contains("PeerCommand.moved(mint: peer)"),
            "the controller builds the mint argv by hand instead of through the one factory "
                + "that writes it")
        XCTAssertTrue(
            tab.contains(".sheet(item: $minting) { row in"),
            "nothing presents the link sheet, so the press sets a value and no sheet appears")
        let start = try slice(
            tab, from: "private func startMinting(_ row: PeerRowModel) {",
            to: "/// Ordinary, worth a look")
        XCTAssertTrue(
            start.contains("guard !snapshotMode else { return }"),
            "a render run starts a subprocess: --render-states writes PNGs and runs nothing")
        XCTAssertTrue(
            start.contains("minted = nil"),
            "the sheet opens holding the LAST run's answer, so one row's link is drawn under "
                + "another row's name until the new run lands")
    }

    /// Three screens, and the one with a Copy button is the one with a link.
    ///
    /// A refused mint drawn as an empty box with Copy under it is a press that
    /// puts nothing on the pasteboard and says nothing about why, which is the
    /// same defect the join key sheet was built to close.
    func testTheLinkSheetDrawsARefusalRatherThanAnEmptyBox() throws {
        let tab = try source("apps/macos/Sources/TcrBar/PanelV4/PeersTabV4.swift")
        let sheet = try slice(
            tab, from: "struct PeerMovedSheet: View {", to: "// MARK: - The Trust sheet")
        XCTAssertTrue(
            sheet.contains("case .refused(let said):") && sheet.contains("case .couldNotRun(let said):"),
            "the sheet stopped drawing one of the two ways a mint does not produce a link, so "
                + "one of them is a sheet with nothing in it")
        XCTAssertTrue(
            sheet.contains("if case .minted(let link, _) = outcome {"),
            "Copy is offered whether or not there is a link to copy")
        XCTAssertTrue(
            sheet.contains("NSPasteboard.general.setString(link, forType: .string)"),
            "the sheet has no Copy: a link is a string somebody has to paste into a chat "
                + "window, and it is not selectable from a screenshot")
        XCTAssertFalse(
            sheet.contains("goes stale") || sheet.contains("24 hours"),
            "the sheet spells out how long a link stays good, which tcr already prints: two "
                + "spellings of one number is the one that drifts")
    }

    // MARK: - The invite sheet

    /// The contract in one check: every sentence about what a key is worth,
    /// which paths it carries, and how long it lasts is a line `tcr peer
    /// invite` printed, not a second spelling of it in Swift.
    ///
    /// Both directions, the technique `FleetStatusTests.swift:872-889`
    /// already uses across this boundary, since Swift cannot import Rust and
    /// no shared constant crosses it: the sentences still exist over there,
    /// so this is not pinned to words `tcr` has already moved past, and the
    /// `PeerInviteSheet` slice contains none of them re-spelled.
    func testTheInviteSheetQuotesTheCliRatherThanRespellingIt() throws {
        let mainRs = try source("src/main.rs")
        XCTAssertTrue(
            mainRs.contains("no internet address"),
            "the no-internet-address sentence has moved or been reworded in src/main.rs; "
                + "update this check and confirm PeerInviteSheet still says nothing of its own "
                + "about it")
        XCTAssertTrue(
            mainRs.contains("needs this Mac's router to forward the"),
            "the router-forwarding sentence has moved or been reworded in src/main.rs")
        XCTAssertTrue(
            mainRs.contains("is used or expires"),
            "the join-capable-until sentence has moved or been reworded in src/main.rs")
        let pairRs = try source("src/peer/pair.rs")
        XCTAssertTrue(
            pairRs.contains("one use and ten minutes"),
            "the ten-minutes-one-use sentence has moved or been reworded in src/peer/pair.rs")

        let tab = try source("apps/macos/Sources/TcrBar/PanelV4/PeersTabV4.swift")
        let sheet = try slice(
            tab, from: "struct PeerInviteSheet: View {", to: "// MARK: - The link sheet")
        for word in ["ten minutes", "one use", "expires", "internet address", "router"] {
            XCTAssertFalse(
                sheet.contains(word),
                "PeerInviteSheet says \"\(word)\" itself, which is a second spelling of a "
                    + "fact tcr already printed and free to drift from it")
        }
    }

    /// A render run mints nothing: `--render-states` writes PNGs and runs no
    /// subprocess, the same guard every other press on this tab carries.
    func testTheInvitePressMintsNothingInARenderRun() throws {
        let tab = try source("apps/macos/Sources/TcrBar/PanelV4/PeersTabV4.swift")
        let start = try slice(
            tab, from: "private func startInviting() {", to: "private func startMinting")
        XCTAssertTrue(
            start.contains("guard !snapshotMode else { return }"),
            "a render run starts a subprocess: --render-states writes PNGs and runs nothing")
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
