import XCTest

@testable import TcrBarCore

/// The Peers panel's wiring: the parts that need a SwiftUI view to observe.
///
/// Source-reading, the technique `PeersPanelWiringTests` and
/// `FleetViewContextMenuTests` already use here and for the same reason:
/// `Package.swift:39-43` gives the test target `TcrBarCore` alone, the views
/// are in the executable, and ViewInspector is not a dependency. Everything
/// that COULD be a value is one: the argv, the sentences, the decode and the
/// arithmetic are `PeerLeaseTests`, `PeerAdmissionTests`,
/// `PeerListDocumentTests`, `PeerJoinLinkTests` and `PeerPaneLayoutTests`.
/// What is left is which view calls which of them, plus two places where one
/// string has to be the SAME string in two files.
///
/// A failure here is usually an ANCHOR that moved rather than a regression:
/// each message says which.
final class PeersPanelViewWiringTests: XCTestCase {

    // MARK: - Item 1: the short pane

    /// The pane's own height arithmetic has a CALLER. `PeerPaneLayout` is
    /// gated by `PeerPaneLayoutTests`, and without a caller every figure in it
    /// would be arithmetic about a pane nothing consults, which is the exact
    /// finding an earlier review recorded against `PeerPanelHeight`.
    func testThePaneConsultsPeerPaneLayout() throws {
        let pane = try source("apps/macos/Sources/TcrBar/PeersSettingsView.swift")
        XCTAssertTrue(
            pane.contains("PeerPaneLayout.height("),
            "PeersSettingsPane no longer asks PeerPaneLayout how tall it is, so the 500 pt "
                + "budget is back to being a number only a test knows about")
        let harness = try source("apps/macos/Sources/TcrBar/RenderSettings.swift")
        XCTAssertTrue(
            harness.contains("PeersSettingsPane.estimatedHeight("),
            "the render harness sizes the Peers window by some other means again, the "
                + "window then holds whatever it used to, not what the pane draws")
    }

    /// The three sections and the disclosure, in the mockup's order. The first
    /// pane had four sections with every control at the top level, which is
    /// what "way too long, almost unusable" was about.
    func testThePaneIsThreeSectionsAndOneDisclosure() throws {
        let pane = try source("apps/macos/Sources/TcrBar/PeersSettingsView.swift")
        let body = try slice(pane, from: "            } else {", to: "        .formStyle(")
        for section in ["thisMac", "sharing", "trustedMacs", "advanced"] {
            XCTAssertTrue(
                body.contains(section),
                "the pane's body no longer draws \(section)")
        }
        XCTAssertFalse(
            body.contains("sharingDefaults"),
            "the six default-lease numbers are back on the pane's top level; they belong in "
                + "the Customize sheet (scene 62), which is what keeps the pane short")
        XCTAssertTrue(
            pane.contains("DisclosureGroup(isExpanded: $advancedOpen)"),
            "Advanced is not a disclosure any more, open by default it is 865 px, which is "
                + "365 px past the hole")
        XCTAssertTrue(
            pane.contains("@State private var advancedOpen = false"),
            "the disclosure no longer starts CLOSED, which is the state the pane's fit is "
                + "measured in")
    }

    // MARK: - Item 5: the render harness aims at an element

    /// The harness asks the PANE what to scroll to, and the pane answers off
    /// the metrics that are gated.
    ///
    /// Aiming at a named element was tried and measured impossible in that
    /// process, no view carries a SwiftUI accessibility identifier and the AX
    /// tree is empty without a client attached, so what makes the target
    /// derivable is structural: the `Advanced…` disclosure is the LAST row, so
    /// the bottom of the document IS the element. This test is that the two
    /// sides of that deal both still hold.
    func testTheHarnessTakesItsScrollTargetFromThePane() throws {
        let pane = try source("apps/macos/Sources/TcrBar/PeersSettingsView.swift")
        let harness = try source("apps/macos/Sources/TcrBar/RenderSettings.swift")
        XCTAssertTrue(
            pane.contains("static var lastRowHeight: CGFloat { shippedMetrics.disclosureHeight }"),
            "the pane no longer tells the harness how tall its last row is, or answers with "
                + "something other than the metrics PeerPaneLayoutTests gates")
        XCTAssertTrue(
            harness.contains("tab == .peers ? PeersSettingsPane.lastRowHeight : nil"),
            "the harness names its own target again rather than asking the pane, which is "
                + "how a number aimed at a moving row went stale in the first place")
        XCTAssertTrue(
            harness.contains("targetMinY: max(0, documentHeight - rowHeight)"),
            "the target is no longer derived from the document's own height, so a pane that "
                + "grows or shrinks is aimed at with yesterday's geometry")
    }

    /// And no code aims at a constant any more.
    ///
    /// Scoped to the FUNCTION, not the file: the doc-comment above it quotes
    /// the retired `tab == .peers ? 470 : 0` on purpose, it is the reason the
    /// element-based aim exists, and deleting the sentence would leave the
    /// next reader to rediscover why a constant went wrong. A whole-file grep
    /// for `470` would therefore be a gate that its own explanation fails.
    func testTheHardCodedScrollOffsetIsGone() throws {
        let harness = try source("apps/macos/Sources/TcrBar/RenderSettings.swift")
        XCTAssertFalse(
            harness.contains("func scrollTop("),
            "the harness has a scrollTop() again: a fixed offset, which against a pane that "
                + "fits scrolls into the bounce region and captures white")
        let target = try slice(
            harness,
            from: "private static func scrollTargetHeight(for tab: SettingsTab) -> CGFloat? {",
            to: "}")
        XCTAssertFalse(
            target.contains("470"),
            "the scroll target is a number again rather than the element it needs in frame")
        XCTAssertTrue(
            harness.contains("RenderScrollTarget.offset("),
            "the offset is no longer derived from where the element actually is")
    }

    // MARK: - Item 2: the pending row and the limited footer

    /// Every verb on a pairing row takes the INSTANCE ID. The proposed name is
    /// a string a stranger on this network chose, and two knocks can propose
    /// the same one.
    func testTheTabsPairingRowRunsEveryVerbAgainstTheInstanceId() throws {
        let tab = try source("apps/macos/Sources/TcrBar/PanelV4/PeersTabV4.swift")
        let card = try slice(
            tab, from: "private func knockCard(", to: "/// `12 shown, 3 more not shown`")
        for verb in ["accept", "ignore", "block"] {
            XCTAssertTrue(
                card.contains("PeerCommand.\(verb)(instance: knock.instanceId)"),
                "the \(verb) control on a pairing row is not handed the instance id")
        }
        // The knock title is split into a name line and an address line:
        // ``PeerAdmission/knockTitle(_:)`` (one line, truncates the address
        // mid-digit at this card's width) is replaced by
        // ``PeerAdmission/knockNameLine(_:)`` and
        // ``PeerAdmission/knockAddressLine(_:)``, drawn on two lines so
        // neither the name nor the address is the one a single-line control
        // truncates. Both still come from `PeerAdmission`, so the card still
        // cannot phrase the request on its own.
        for helper in ["knockNameLine(knock)", "knockAddressLine(knock)"] {
            XCTAssertTrue(
                card.contains("PeerAdmission.\(helper)"),
                "the row writes its own title again, so the tab and the pane can phrase one "
                    + "request two ways: missing PeerAdmission.\(helper)")
        }
    }

    /// The footer is drawn, and from the snapshot's own derivation rather than
    /// a count this view computes.
    func testTheTabDrawsTheLimitedFooterFromTheSnapshot() throws {
        let tab = try source("apps/macos/Sources/TcrBar/PanelV4/PeersTabV4.swift")
        XCTAssertTrue(
            tab.contains("if let footer = snapshot.limitedFooter {"),
            "the tab no longer draws the \"N more not shown\" footer, so a list capped at 12 "
                + "looks exactly like a network with 12 Macs on it")
        XCTAssertTrue(
            tab.contains("PeerAdmission.limitedFooter(shown: rows.count, limited: limited)"),
            "the footer's sentence is computed somewhere other than PeerAdmission, which is "
                + "where the gate for it is")
    }

    // MARK: - Items 6 and 7: the two sheets

    /// The per-Mac sheet's controls each write a WHOLE lease.
    ///
    /// One write per lease, because the scope popup and the numbers popup are
    /// two controls over one record: a partial write would leave the peers file
    /// holding half of one lease and half of the last.
    func testTheMacSheetsLeaseRowWritesEveryFlag() throws {
        let pane = try source("apps/macos/Sources/TcrBar/PeersSettingsView.swift")
        let row = try slice(
            pane, from: "private func leaseRow(", to: "private var endChoices")
        XCTAssertTrue(
            row.contains("PeerCommand.lend("),
            "the scope and numbers popups no longer write a lease")
        XCTAssertTrue(
            row.contains("PeerCommand.lendRevoke(peer: peer, leaseId: grant.leaseId)"),
            "Revoke is no longer per lease, revoking the wrong one, or all of them, is the "
                + "failure that replaces it")
        XCTAssertTrue(
            row.contains("PeerCommand.lendRelend(peer: peer, leaseId: grant.leaseId)"),
            "the greyed row lost its Re-lend, which is the whole reason decision row 13 "
                + "keeps an ended lease in the list")
        // ONE writer, asserted as one: the row had this call three times and
        // a mutation that dropped `end:` from one of them left the gate green
        // (this script's third run). Both halves are checked, the closure
        // carries the end, and every popup goes through the closure.
        XCTAssertTrue(
            row.contains("PeerCommand.lend(peer: peer, scope: scope, terms: terms, end: end)"),
            "the row's single lease writer no longer carries the end, and a lend with no "
                + "--for is what \"no end\" means to the CLI: a scope change would silently "
                + "remove the end")
        for popup in [
            "scopeMenuItems { write($0, grant.terms, end) }",
            "write(scope, .standard(for: window), end)",
            "Button(choice.label) { write(scope, grant.terms, choice) }",
        ] {
            XCTAssertTrue(
                row.contains(popup),
                "a popup on the lease row writes the lease its own way again rather than "
                    + "through the row's one writer: \(popup)")
        }
    }

    /// The defaults sheet writes `share --scope …`, and its Cancel writes
    /// nothing.
    func testTheDefaultsSheetWritesTheScopeAndCancelWritesNothing() throws {
        let pane = try source("apps/macos/Sources/TcrBar/PeersSettingsView.swift")
        let sheet = try slice(
            pane, from: "private var defaultsSheet: some View {",
            to: "// MARK: - The trusted-Mac sheet")
        XCTAssertTrue(
            sheet.contains("PeerCommand.shareDefaults(scope: defaultScope, terms: terms)"),
            "the Customize sheet no longer writes decision row 12's scope for the default "
                + "lease")
        let cancel = try slice(
            sheet, from: "Button(\"Cancel\", role: .cancel)", to: "Button(\"Done\")")
        XCTAssertFalse(
            cancel.contains("controller.run"),
            "Cancel writes something, a dismissable sheet's Cancel must run no verb at all")
    }

    // MARK: - Item 8: the account card's line

    /// The card looks its leases up through the helper that REFUSES the masked
    /// key, not with a subscript.
    ///
    /// `tcr peer ls --json` masks any account label its sanitizer rejects, and
    /// an email is rejected, so every email-labelled account inside a lease
    /// arrives under one shared `[masked]` key. A subscript would hand those
    /// rows to whichever card asked, saying a different account's allowance
    /// is being spent.
    func testTheAccountCardLooksUpItsLeasesThroughTheGuardedHelper() throws {
        let tab = try source("apps/macos/Sources/TcrBar/PanelV4/AccountsTabV4.swift")
        XCTAssertTrue(
            tab.contains("lentTo: PeerLease.leases(forAccountLabel: row.account.name"),
            "the accounts tab indexes the lentTo map directly again, which attributes one "
                + "account's lease to another whenever the label was masked")
        let card = try source("apps/macos/Sources/TcrBar/PanelV4/AccountCard.swift")
        XCTAssertTrue(
            card.contains("if let line = PeerLease.lentToLine(lentTo) {"),
            "the card no longer draws decision row 13's Lent to line")
        XCTAssertTrue(
            card.contains("onOpenLender(first)"),
            "the line is a readout only, scene 64 makes it a control that opens that Mac's "
                + "sheet")
    }

    /// ONE peer read behind both surfaces. Two controllers would be two reads
    /// of one file at two instants.
    func testTheAccountsTabAndThePeersTabShareOnePeerController() throws {
        let fleet = try source("apps/macos/Sources/TcrBar/FleetView.swift")
        XCTAssertTrue(
            fleet.contains("@StateObject private var peers = PeerController()"),
            "FleetView no longer owns the peer reader, so the accounts tab has no lentTo to "
                + "draw from")
        XCTAssertTrue(
            fleet.contains("PeersView(\n                    controller: peers,"),
            "the Peers tab makes its own controller again, two readers, two instants of one "
                + "file, and a footer that can disagree with the rows above it")
        XCTAssertTrue(
            fleet.contains("lentTo: peers.snapshot.lentTo"),
            "the accounts tab is not handed the lentTo map")
    }

    // MARK: - Items 4 and 9: the tcr:// link

    /// Both schemes are registered, and they are two entries rather than one.
    ///
    /// The plist is written by the build script, so this reads the SCRIPT. The
    /// artifact is what LaunchServices sees, and only building the bundle
    /// proves that, which is the gap this test names rather than papers over.
    func testTheBundleRegistersBothUrlSchemes() throws {
        let script = try source("apps/macos/scripts/build-tcrbar.sh")
        XCTAssertTrue(
            script.contains("url_scheme=\"tcrbar\""),
            "the app's own scheme is gone; `tcr` could no longer ask it to check for updates")
        XCTAssertTrue(
            script.contains("peer_url_scheme=\"tcr\""),
            "the tcr:// scheme is not registered, so a share link opens nothing at all and "
                + "the app-side handler is unreachable")
        XCTAssertTrue(
            script.contains("<string>$peer_url_scheme</string>"),
            "peer_url_scheme is set but never written into CFBundleURLTypes, the variable "
                + "would be a comment with a value")
    }

    /// The handler passes the WHOLE link on stdin and logs neither the link
    /// nor a key.
    func testTheUrlHandlerPassesTheWholeLinkAndLogsNeitherItNorAKey() throws {
        let app = try source("apps/macos/Sources/TcrBar/TcrBarApp.swift")
        let handler = try slice(
            app, from: "private func handleJoinLink(", to: "private func handle(")
        XCTAssertTrue(
            handler.contains("PeerController.join(link: url)"),
            "the handler unpacks the link itself again, a second parser of one string, free "
                + "to disagree with the CLI that ships")
        XCTAssertTrue(
            handler.contains("PeerJoinLink.redacted(url)"),
            "the handler logs something other than the redacted shape")
        XCTAssertFalse(
            handler.contains("url.absoluteString"),
            "the link's own text is in the handler, which is one edit away from a log line "
                + "or an argument carrying the network key")
        XCTAssertTrue(
            app.contains("if url.scheme?.lowercased() == PeerJoinLink.scheme {"),
            "tcr:// is no longer routed to the join handler, so it falls through to the "
                + "update-check switch and is logged as unhandled")
    }

    /// The Advanced pane's caps rows come from the caps OBJECT, and the pane
    /// holds no second copy of the numbers.
    func testTheAdvancedPaneReadsTheCapsObject() throws {
        let pane = try source("apps/macos/Sources/TcrBar/PeersSettingsView.swift")
        XCTAssertTrue(
            pane.contains("if let caps = snapshot.caps {"),
            "the Advanced pane no longer reads the caps tcr reported")
        XCTAssertTrue(
            pane.contains("ForEach(caps.advancedRows, id: \\.label)"),
            "the caps rows are written out by hand again, which is six numbers in two places")
        let caps = try slice(
            pane, from: "@ViewBuilder private var capsRows: some View {",
            to: "@ViewBuilder private var blockedList: some View {")
        for number in ["\"12\"", "\"16\"", "\"8\""] {
            XCTAssertFalse(
                caps.contains(number),
                "the Advanced pane carries the literal \(number) again, the cap it names is "
                    + "a constant in src/peer, and the copy that drifts is the one nobody runs")
        }
    }

    /// The Blocked list shows both states, with their reasons, and Unblock
    /// takes the ADDRESS.
    func testTheBlockedListShowsBothStatesAndUnblocksByAddress() throws {
        let pane = try source("apps/macos/Sources/TcrBar/PeersSettingsView.swift")
        let list = try slice(
            pane, from: "@ViewBuilder private var blockedList: some View {",
            to: "// MARK: - The Sharing defaults sheet")
        XCTAssertTrue(
            list.contains("PeerCommand.unblock(address: ban.addr)"),
            "Unblock is handed something other than the address tcr peer ls lists under "
                + "blocked")
        // `now:` and not `Date()`: the pane has ONE clock accessor,
        // which is the real instant except under `--render-settings`, where a
        // pane judging fixture ends against today read every one of them as
        // expired. The spelling these two assertions pinned was incidental;
        // what they are about is that the row says why, and that mutes are
        // listed beside bans.
        XCTAssertTrue(
            list.contains("PeerAdmission.blockSentence(ban, now: now)"),
            "a blocked row no longer says WHY it is blocked")
        XCTAssertTrue(
            list.contains("PeerAdmission.muteSentence(mute, now: now)"),
            "the muted addresses are gone from the list, so an operator looking for \"why is "
                + "that Mac not appearing\" cannot tell the two states apart")
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

    /// The text between two anchors, so an assertion cannot match an identical
    /// line elsewhere in a thousand-line view.
    private func slice(_ contents: String, from: String, to: String) throws -> String {
        guard let start = contents.range(of: from) else {
            throw Anchor.missing(from)
        }
        let rest = contents[start.upperBound...]
        guard let end = rest.range(of: to) else {
            throw Anchor.missing(to)
        }
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
