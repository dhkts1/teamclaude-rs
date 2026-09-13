import XCTest

/// Controls the v4 panel has to keep drawing.
///
/// Source-reading, the technique `FleetViewContextMenuTests` already uses here
/// for assertions a runtime test cannot reach cheaply: these views are built
/// from live `AccountController`/`GroupController`/`RemoveAccountController`
/// dependencies with no test doubles in this target, and ViewInspector is not a
/// dependency. The renders in `--render-states` are the second instrument —
/// `04b-needs-relogin-row` and `19-accounts-tab-parity` show both of these — and
/// this is the one that fails in CI when someone takes them out again.
final class PanelV4ControlsTests: XCTestCase {

    // MARK: - Review #1: per-account actions are visible, not right-click only

    /// `.contextMenu` was the card's ONLY interaction: seven per-account
    /// actions, two of them destructive, reachable by pointer alone.
    func testTheCardDrawsItsActionsAsVisibleControls() throws {
        let source = try panelSource("PanelV4/AccountsTabV4.swift")
        XCTAssertTrue(
            source.contains("@ViewBuilder var actions: (Account) -> Actions"),
            "AccountsTabV4 no longer takes a visible-actions slot")
        XCTAssertTrue(
            source.contains(
                "AccountCard(account: row.account, shape: shape, now: now) "
                    + "{ actions(row.account) }"),
            "the card is no longer handed its actions")
        XCTAssertTrue(
            source.contains(".contextMenu { menu(row.account) }"),
            "the context menu is the SECOND route and stays")
    }

    /// A card reading NEEDS RE-LOGIN with no visible way to repair itself is the
    /// state the panel exists to get an operator out of.
    func testABrokenCardDrawsTheReloginButton() throws {
        let source = try panelSource("FleetView.swift")
        XCTAssertTrue(
            source.contains(
                """
                if account.health == .needsRelogin {
                                reloginButton
                            }
                            accountActionsMenu
                """.trimmingCharacters(in: .whitespacesAndNewlines)),
            "the v4 card's action cluster no longer draws reloginButton ahead of "
                + "the actions menu on a broken account")
    }

    /// Both slots come from ONE `AccountRow`, so the visible controls and the
    /// context menu can never be wired to different controllers.
    func testBothCardSlotsAreBuiltFromTheSameRow() throws {
        let source = try panelSource("FleetView.swift")
        XCTAssertTrue(source.contains("v4AccountRow(account, in: fleet, menuOnly: true)"))
        XCTAssertTrue(source.contains("v4AccountRow(account, in: fleet, actionsOnly: true)"))
    }

    // MARK: - Review #12: the disclosure never vanishes

    /// Expanding used to be a one-way door — both `V4Disclosure` call sites sat
    /// behind `if summarised` / `if hidden > 0`, so once a group was open
    /// neither rendered. The state persists to `UserDefaults` across launches,
    /// and the control that disappeared was the one the user had just pressed.
    func testTheDisclosureIsDrawnInBothDirections() throws {
        let source = try panelSource("PanelV4/AccountsTabV4.swift")
        XCTAssertTrue(
            source.contains("if isCollapsible(section) {"),
            "the disclosure is gated on something other than the section's own "
                + "ability to hide rows")
        XCTAssertFalse(
            source.contains("if hidden > 0 {"),
            "the one-way gate is back: a fully expanded group has hidden == 0 and "
                + "draws no way back")
        XCTAssertTrue(
            source.contains("title: disclosureTitle(section, expanded: expanded, hidden: hidden)"),
            "the disclosure's title no longer depends on which way it will move")
        XCTAssertTrue(
            source.contains("expanded: expanded,"),
            "the chevron is no longer told the state, so it points down on a group "
                + "the press will CLOSE")
    }

    func testTheClosingDirectionHasItsOwnWords() throws {
        let source = try panelSource("PanelV4/AccountsTabV4.swift")
        XCTAssertTrue(
            source.contains(
                """
                if expanded { return "Show fewer accounts" }
                """))
        XCTAssertTrue(
            source.contains("\"Collapses this group again.\""),
            "the reverse path lost the hint the pre-v4 legend button carried")
    }

    /// A wholly-parked group of three or fewer hides nothing, so it gets no
    /// control: one that does nothing is worse than none.
    func testAGroupThatHidesNothingGetsNoDisclosure() throws {
        let source = try panelSource("PanelV4/AccountsTabV4.swift")
        XCTAssertTrue(
            source.contains(
                "|| (section.isWhollyParked && section.rows.count > Self.parkedVisibleRows)"))
    }

    // MARK: - Review #13: a truncating role always offers its full value

    /// `rg '\\.help\\(' PanelV4/` returned three hits before this, all on
    /// Buttons, and `rg textSelection PanelV4/` returned none — while 8 of 8
    /// mono command rows on the Tools tab end in an ellipsis.
    func testEveryTruncatingTextRoleOffersItsFullValue() throws {
        // CODE only. A bare substring count over the whole file counts the
        // doc-comment that NAMES the modifier, which is how this assertion
        // first read 5 against four call sites.
        let source = try panelCode("PanelV4/V4Text.swift")
        XCTAssertEqual(
            source.components(separatedBy: ".help(text)").count - 1, 4,
            "one of NameText / DimText / MuteText / MonoText no longer offers its "
                + "full value on hover")
        XCTAssertEqual(
            source.components(separatedBy: ".accessibilityValue(text)").count - 1, 4,
            "one of the four roles no longer speaks its full value")
    }

    /// The file with every comment line removed, for an assertion that COUNTS
    /// occurrences rather than merely finding one.
    private func panelCode(_ relative: String) throws -> String {
        try panelSource(relative)
            .split(separator: "\n", omittingEmptySubsequences: false)
            .filter { !$0.trimmingCharacters(in: .whitespaces).hasPrefix("//") }
            .joined(separator: "\n")
    }

    /// A command you cannot read is one thing; a command you cannot copy is
    /// another. The mockup carries the full command as a `title=` on 7 of 7
    /// mono spans.
    func testTheMonoRoleIsSelectable() throws {
        let source = try panelSource("PanelV4/V4Text.swift")
        XCTAssertTrue(source.contains(".textSelection(.enabled)"))
    }

    /// The panel's only privacy claim read "from request bodies only · nothing
    /// lo…" while 35.5 pt of the row sat empty.
    func testTheFooterOffersItsFullLeadingRun() throws {
        let source = try panelSource("PanelV4/PanelFooter.swift")
        XCTAssertTrue(source.contains(".help(leading)"))
        XCTAssertTrue(source.contains(".accessibilityValue(leading)"))
    }

    // MARK: - Review #10: whitespace collapses before a character does

    func testTheRowGivesItsLeadingSlotPriority() throws {
        let source = try panelSource("PanelV4/V4Card.swift")
        XCTAssertTrue(source.contains(".layoutPriority(1)"))
        XCTAssertTrue(
            source.contains("Spacer(minLength: 0)"),
            "a Spacer is a view, so the stack's spacing is charged on both sides "
                + "of it; a minLength of rowGap made the real gap rowGap * 3")
    }

    func testTheHeaderAndFooterGiveTheirLeadingSlotPriority() throws {
        for file in ["PanelV4/PanelHeader.swift", "PanelV4/PanelFooter.swift"] {
            let source = try panelSource(file)
            XCTAssertTrue(source.contains(".layoutPriority(1)"), "\(file)")
            XCTAssertTrue(source.contains("Spacer(minLength: 0)"), "\(file)")
        }
    }

    private func panelSource(_ relative: String) throws -> String {
        let repoRoot = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent()  // -> TcrBarTests
            .deletingLastPathComponent()  // -> Tests
            .deletingLastPathComponent()  // -> apps/macos
            .deletingLastPathComponent()  // -> apps
            .deletingLastPathComponent()  // -> repo root
        let file = repoRoot.appendingPathComponent("apps/macos/Sources/TcrBar/\(relative)")
        return try String(contentsOf: file, encoding: .utf8)
    }
}
