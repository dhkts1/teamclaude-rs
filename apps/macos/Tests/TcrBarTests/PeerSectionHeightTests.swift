import XCTest

@testable import TcrBarCore

/// The height gate: seven peers still clear the 520 pt cap, and growth past
/// it is charged to the peer list rather than to the two switches or the
/// footer.
///
/// The figures below are the ones `PeersTabV4.heightMetrics` passes in, but
/// they are transcribed HERE as literals rather than read from `V4`, for the
/// reason `PanelSize.Geometry` documents: `V4` is in the executable target and
/// this target cannot link it, and a test that restated a clamp would prove an
/// identity about a number instead of testing the rule.
final class PeerSectionHeightTests: XCTestCase {

    /// `V4`'s Comfortable values: `lineHeight(15) = 21`, `lineHeight(12) = 17`,
    /// a meter at one bar (7) plus its top margin (3) plus its value line (17),
    /// a card's two 11 pt insets, `cardGap` 14.
    ///
    /// `fixedChrome` is the two switch cards, the section head and the count
    /// line: two cards of a name line, a sub line and a two-line "what yes
    /// does" block, plus a 15 pt section head and a 17 pt count line.
    ///
    /// `knockCardHeight` is one knock card as `PeersTabV4.heightMetrics`
    /// derives it at this density: a 21 pt name line, four 17 pt mute lines
    /// (the address row and the three the sentence wraps to), three 3 pt gaps
    /// between the card's four children, a 40 pt button row and the card's
    /// own 22 pt of chrome.
    private let comfortable = PeerPanelHeight.Metrics(
        nameLineHeight: 21,
        subLineHeight: 17,
        meterHeight: 27,
        cardChrome: 22,
        cardGap: 14,
        fixedChrome: 216,
        knockCardHeight: 160
    )

    /// The same tab at Compact, which is what `.auto` resolves to above four
    /// accounts and therefore what this panel usually draws:
    /// `lineHeight(13) = 18`, `lineHeight(11) = 15`, two 8 pt insets, an 8 pt
    /// gap.
    private let compact = PeerPanelHeight.Metrics(
        nameLineHeight: 18,
        subLineHeight: 15,
        meterHeight: 23,
        cardChrome: 16,
        cardGap: 8,
        fixedChrome: 180,
        knockCardHeight: 133
    )

    private func geometry(panelWidth: CGFloat = 372) -> PanelSize.Geometry {
        PanelSize.Geometry(
            panelWidth: panelWidth,
            gutter: 10,
            sectionSpacing: 8,
            lineSpacing: 4,
            rowSpacing: 14,
            hairlineWidth: 0.5,
            rowHeight: 60,
            fixedControlsHeight: 96
        )
    }

    /// One line is one line, so a part's height is countable rather than
    /// guessed, the same shape of fake `PanelSizeTests` uses.
    private struct FixedMetrics: TextMetrics {
        let perLine: CGFloat
        func height(of text: String, fontSize: CGFloat, width: CGFloat) -> CGFloat { perLine }
    }

    private func trusted(_ count: Int) -> [PeerPanelHeight.Row] {
        Array(repeating: .plain, count: count)
    }

    // MARK: - The cap

    /// The tab's own budget figure: seven peers clear the 520 pt cap at
    /// Comfortable. Measured, and it is TIGHT, seven rows of 60 pt with a
    /// 14 pt gap each is 518 pt, two points under, so the assertion carries
    /// the number rather than only the verdict. An eighth row at this density
    /// does NOT fit, which is why the mockup's own rule 7 ("eight Macs still
    /// clear the 520 pt list cap") is asserted at Compact below instead of
    /// here: that claim is true at the density this panel usually draws and
    /// false at the roomy one.
    func testSevenPeersClearTheCapAtComfortable() {
        XCTAssertEqual(PeerPanelHeight.listHeight(rows: trusted(7), metrics: comfortable), 518)
        XCTAssertFalse(PeerPanelHeight.overflows(rows: trusted(7), metrics: comfortable))
        XCTAssertEqual(
            PeerPanelHeight.peerSectionHeight(rows: trusted(7), metrics: comfortable),
            comfortable.fixedChrome + 518,
            "under the cap the list gets exactly what it wants")
    }

    /// The mockup's rule 7, at the density it holds for.
    func testEightPeersClearTheCapAtCompact() {
        XCTAssertFalse(PeerPanelHeight.overflows(rows: trusted(8), metrics: compact))
        XCTAssertEqual(PeerPanelHeight.listHeight(rows: trusted(8), metrics: compact), 456)
    }

    /// Every row metered (the tallest shape the tab can draw) still clears
    /// the cap at four peers, the "4 peers with path sub-lines" case.
    func testFourMeteredPeersClearTheCap() {
        let rows = Array(repeating: PeerPanelHeight.Row.metered, count: 4)
        XCTAssertFalse(PeerPanelHeight.overflows(rows: rows, metrics: comfortable))
    }

    /// Forty peers overflow, and the LIST is clamped to the cap itself, not
    /// to the cap plus a row, and not to what forty rows want.
    func testFortyPeersClampTheListToTheCap() {
        let rows = trusted(40)
        XCTAssertTrue(PeerPanelHeight.overflows(rows: rows, metrics: comfortable))
        XCTAssertEqual(
            PeerPanelHeight.peerSectionHeight(rows: rows, metrics: comfortable),
            comfortable.fixedChrome + PanelHeight.panelMaxHeight)
    }

    // MARK: - The drawn viewport

    /// ``PeerPanelHeight/listViewportHeight(rows:metrics:)`` is the number
    /// `PeersTabV4` frames its `ScrollView` to, so it is the one figure in
    /// this file that has a drawn consequence: under the cap it is what the
    /// rows want, and at forty peers it is the cap exactly.
    func testTheViewportIsTheRowsUnderTheCapAndTheCapAboveIt() {
        XCTAssertEqual(
            PeerPanelHeight.listViewportHeight(rows: trusted(7), metrics: comfortable), 518,
            "seven peers fit, so the viewport is not clamped at all")
        XCTAssertEqual(
            PeerPanelHeight.listViewportHeight(rows: trusted(40), metrics: comfortable),
            PanelHeight.panelMaxHeight,
            "forty peers scroll inside the cap")
    }

    /// The viewport and the section height cannot disagree about the cap,
    /// which is why the `min` lives in one function and not at the call site.
    func testTheSectionIsItsChromePlusTheViewport() {
        for count in [0, 1, 7, 40] {
            let rows = trusted(count)
            XCTAssertEqual(
                PeerPanelHeight.peerSectionHeight(rows: rows, metrics: comfortable),
                comfortable.fixedChrome
                    + PeerPanelHeight.listViewportHeight(rows: rows, metrics: comfortable),
                "\(count) peers")
        }
    }

    // MARK: - Where the growth is charged

    /// The property this whole file exists for: at forty peers the footer is
    /// the same height it is at one, the two switches are still fully
    /// reserved, and the panel's growth stops at the cap. Capping the TOTAL
    /// instead is the recorded bug (`ffe8a86`), it takes the difference out
    /// of whatever is drawn last, which is the footer.
    func testGrowthIsChargedToTheListNeverToTheFooter() {
        let header = [PanelSize.Line("13 accounts, 6 with headroom", fontSize: 15)]
        let footer = [PanelSize.Line("2 Macs found, 1 trusted", fontSize: 12.5)]
        let text = FixedMetrics(perLine: 18)

        let small = PeerPanelHeight.plan(
            rows: trusted(1), header: header, footer: footer,
            geometry: geometry(), metrics: comfortable, textMetrics: text)
        let large = PeerPanelHeight.plan(
            rows: trusted(40), header: header, footer: footer,
            geometry: geometry(), metrics: comfortable, textMetrics: text)

        XCTAssertEqual(large.footerHeight, small.footerHeight)
        XCTAssertEqual(large.headerHeight, small.headerHeight)
        XCTAssertEqual(large.frameHeight, small.frameHeight)
        XCTAssertEqual(large.listHeight, comfortable.fixedChrome + PanelHeight.panelMaxHeight)
        XCTAssertEqual(
            large.height - small.height, large.listHeight - small.listHeight,
            "every point of growth landed in the peer list")
        XCTAssertGreaterThan(
            large.listHeight, PanelHeight.panelMaxHeight,
            "the switches are reserved on top of the capped list, not inside it")
    }

    /// The five-part identity with every part above zero, the shape
    /// `PanelSizeTests` holds `PanelSize.Plan` to, restated for the peers
    /// content region so a part that quietly went to zero cannot pass.
    func testTheIdentityHoldsWithEveryPartAboveZero() {
        let plan = PeerPanelHeight.plan(
            rows: trusted(40),
            header: [PanelSize.Line("13 accounts, none with headroom", fontSize: 15)],
            footer: [PanelSize.Line("2 Macs found, 2 trusted", fontSize: 12.5)],
            geometry: geometry(), metrics: comfortable, textMetrics: FixedMetrics(perLine: 18))

        XCTAssertGreaterThan(plan.headerHeight, 0)
        XCTAssertGreaterThan(plan.footerHeight, 0)
        XCTAssertGreaterThan(plan.frameHeight, 0)
        XCTAssertGreaterThan(plan.listHeight, 0)
        XCTAssertEqual(
            plan.height,
            plan.headerHeight + plan.footerHeight + plan.frameHeight + plan.listHeight)
    }

    /// The Peers tab is as wide as the panel and no wider: the plan carries
    /// the geometry's own width through, it does not author one.
    ///
    /// The panel is **372** pt (`Tokens.swift:425`, `Tok.panelWidth`): the
    /// mockups were drawn at 380 and the v4 migration moved the panel to
    /// 372, so 380 is the
    /// picture's width and not the app's. Nothing here changes `Tok`; this
    /// asserts the pass-through at the shipped number rather than at the
    /// mockup's.
    func testThePeersTabIsAsWideAsThePanel() {
        let plan = PeerPanelHeight.plan(
            rows: trusted(2), header: [], footer: [],
            geometry: geometry(panelWidth: 372), metrics: comfortable,
            textMetrics: FixedMetrics(perLine: 18))
        XCTAssertEqual(plan.width, 372)
        XCTAssertEqual(plan.contentSize.width, 372)
    }

    // MARK: - Rows and zero peers

    /// A metered row is taller than a plain one by exactly its meter and its
    /// second sub line, so a row shape cannot gain height from nowhere.
    func testAMeteredRowCostsItsMeterAndOneMoreSubLine() {
        XCTAssertEqual(
            PeerPanelHeight.rowHeight(.metered, metrics: comfortable)
                - PeerPanelHeight.rowHeight(.plain, metrics: comfortable),
            comfortable.meterHeight + comfortable.subLineHeight)
    }

    /// No peers is not an empty tab: the two switches are still there, so the
    /// section reserves its fixed chrome and nothing else.
    func testNoPeersStillReservesTheTwoSwitches() {
        XCTAssertEqual(
            PeerPanelHeight.peerSectionHeight(rows: [], metrics: comfortable),
            comfortable.fixedChrome)
    }

    // MARK: - A Mac asking to connect

    /// **The charge this file was missing.** A section holding one pending
    /// knock is taller than the same section holding none by exactly that
    /// card and the gap above it.
    ///
    /// It was charged nowhere: `fixedChrome` is the two switches, the section
    /// head and the count line, and the list is the trusted rows, so a panel
    /// with a request to connect in it was sized for a panel without one.
    /// That is vertical pressure nothing in the budget can see, and what gave
    /// way under it was the card's own help line, cut mid-word.
    ///
    /// Watched red: with `peerSectionHeight` ignoring its `pendingKnocks`
    /// argument the two sides are equal and this fails at the first assertion.
    func testOnePendingKnockCostsItsCardAndTheGapAboveIt() {
        let rows = trusted(3)
        let none = PeerPanelHeight.peerSectionHeight(rows: rows, metrics: comfortable)
        let one = PeerPanelHeight.peerSectionHeight(
            rows: rows, pendingKnocks: 1, metrics: comfortable)
        XCTAssertEqual(
            one - none, comfortable.knockCardHeight + comfortable.cardGap,
            "a knock card is drawn and paid for nowhere")
        XCTAssertEqual(
            PeerPanelHeight.peerSectionHeight(rows: rows, pendingKnocks: 2, metrics: comfortable)
                - one,
            comfortable.knockCardHeight + comfortable.cardGap,
            "the second Mac asking at once costs the same as the first")
    }

    /// The knock cards are charged OUTSIDE the capped list. Forty peers clamp
    /// the list; a knock on top of them still adds its own card, because it is
    /// drawn above "Other Macs" and outside the scroll view.
    func testAKnockIsChargedOutsideTheCappedList() {
        let rows = trusted(40)
        XCTAssertEqual(
            PeerPanelHeight.peerSectionHeight(rows: rows, pendingKnocks: 1, metrics: comfortable),
            comfortable.fixedChrome + comfortable.knockCardHeight + comfortable.cardGap
                + PanelHeight.panelMaxHeight,
            "a knock arriving under forty peers was absorbed by the clamp, so the card "
                + "would be drawn in points nothing reserved")
    }

    /// And the plan carries it through to the drawn region, so the panel the
    /// shell sizes is the panel the tab draws.
    func testThePlanCarriesThePendingKnocksThrough() {
        func plan(_ knocks: Int) -> PanelSize.Plan {
            PeerPanelHeight.plan(
                rows: trusted(2), pendingKnocks: knocks,
                header: [PanelSize.Line("13 accounts, 6 with headroom", fontSize: 15)],
                footer: [PanelSize.Line("2 Macs found, 1 trusted", fontSize: 12.5)],
                geometry: geometry(), metrics: comfortable,
                textMetrics: FixedMetrics(perLine: 18))
        }
        XCTAssertEqual(
            plan(1).listHeight - plan(0).listHeight,
            comfortable.knockCardHeight + comfortable.cardGap)
        XCTAssertEqual(
            plan(1).footerHeight, plan(0).footerHeight,
            "the knock came out of the footer, which is the bug this file exists for")
    }
}
