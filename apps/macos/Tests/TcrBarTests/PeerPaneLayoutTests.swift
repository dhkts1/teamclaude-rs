import XCTest

@testable import TcrBarCore

/// The short Settings > Peers pane's height budget.
///
/// # Where these numbers come from, and what they can prove
///
/// They are the MOCKUP's own part heights, read off its CSS
/// (`mockups/settings-peers-short.html`): `.frow` and `.prow` and `.disc` are
/// `min-height:40px` (:180, :199, :207), `.fgrp` is a `.5px` border on each
/// side, `.fsec` is an 11 px caps line with `margin:4px 4px 3px` (:168), and
/// the Advanced group carries `margin-top:5px` (:766). `rowDetailHeight` is
/// the extra an explained row takes when its sentence wraps to two or three
/// lines at 412 px, `.frow .l small` is 11 px at `line-height:1.3` (:184).
///
/// So the first test is a CROSS-CHECK, not a tautology: the mockup measured
/// the whole pane at 493.39 px with its own layout engine, and this
/// arithmetic, given the same parts, has to land on the same number. A
/// structural mistake, a section counted twice, a group's chrome forgotten,
/// the knock row charged to the wrong list, moves the total well outside the
/// tolerance. What it cannot prove is what a SwiftUI `Form` charges for the
/// same rows; that is what `--render-settings` and a person's eye are for, and
/// this file says so rather than implying otherwise.
///
/// The other tests are the ones that hold regardless of any metric: what a
/// peer costs, what a request costs, and that the pane the design replaced was
/// far taller than the hole at the same numbers.
final class PeerPaneLayoutTests: XCTestCase {

    /// The mockup's parts, in its own pixels.
    ///
    /// Six of the eight are read off the CSS and are not adjustable here:
    /// `sectionHeadHeight` is the 11 px caps line plus its 4/3 margins,
    /// `rowHeight`/`peerRowHeight`/`disclosureHeight` are the `min-height:40px`
    /// of `.frow`/`.prow`/`.disc`, `groupChrome` is `.fgrp`'s two `.5px`
    /// borders, and `groupGap` is the `margin-top:5px` between groups.
    ///
    /// The other two, how much a WRAPPED sentence adds to a row, and how tall
    /// the pairing-request row is, depend on where the text breaks at 412 px,
    /// which no CSS line states. They are SOLVED from the mockup's own two
    /// measurements (see
    /// ``testTheWrappedSentenceHeightSolvedFromTheMockupIsPlausible``) rather
    /// than guessed, and that test is the one that would catch a structural
    /// mistake here: a section counted twice moves the solved value out of the
    /// range its own CSS can produce.
    private let mockup = PeerPaneLayout.Metrics(
        sectionHeadHeight: 22, rowHeight: 40, rowDetailHeight: 23, knockRowHeight: 58,
        peerRowHeight: 40, groupChrome: 1, groupGap: 5, disclosureHeight: 40)

    /// Scene 59's two measured totals, from its own `<span class="m">` figures.
    private let measuredWithRequest: CGFloat = 493.39
    private let measuredWithoutRequest: CGFloat = 434.95

    /// Scene 59's own measurement, reproduced from the parts.
    func testTheArithmeticReproducesTheMockupsMeasuredPaneHeight() {
        let computed = PeerPaneLayout.height(
            PeerPaneLayout.Content(trustedMacs: 2, pendingKnocks: 1), metrics: mockup)
        XCTAssertEqual(
            computed, measuredWithRequest, accuracy: 0.02 * measuredWithRequest,
            "the pane's height arithmetic disagrees with the mockup that measured the same "
                + "pane: \(computed) pt computed against \(measuredWithRequest) px measured. "
                + "One of the two models the sections wrongly")
    }

    /// **The structural gate.** Invert the arithmetic: with the six CSS-read
    /// parts fixed, what sentence height would the mockup's own no-request
    /// measurement imply? The pane has three explained rows (Name, Announce
    /// my name, and the Share switch), so the measurement minus everything
    /// else, divided by three, is that number.
    ///
    /// It has to land in the band `.frow .l small` can actually produce: 11 px
    /// at `line-height:1.3` is 14.3 px a line, so one to three lines of
    /// sentence is 14 to 43 px. Outside it, the model is wrong somewhere else
    ///, a group's chrome forgotten, a section counted twice, the pane's four
    /// gaps charged as three, and the total only looked right because two
    /// parameters were free to absorb it.
    func testTheWrappedSentenceHeightSolvedFromTheMockupIsPlausible() {
        var withoutSentences = mockup
        withoutSentences = PeerPaneLayout.Metrics(
            sectionHeadHeight: mockup.sectionHeadHeight, rowHeight: mockup.rowHeight,
            rowDetailHeight: 0, knockRowHeight: mockup.knockRowHeight,
            peerRowHeight: mockup.peerRowHeight, groupChrome: mockup.groupChrome,
            groupGap: mockup.groupGap, disclosureHeight: mockup.disclosureHeight)
        let quiet = PeerPaneLayout.Content(trustedMacs: 2, pendingKnocks: 0)
        let bare = PeerPaneLayout.height(quiet, metrics: withoutSentences)
        let solved = (measuredWithoutRequest - bare) / 3

        XCTAssertGreaterThan(
            solved, 14,
            "the mockup's 434.95 px leaves \(solved) px for each of the pane's three "
                + "explained rows, which is less than one line of its 11 px/1.3 sentence: "
                + "the model is charging for something the pane does not draw")
        XCTAssertLessThan(
            solved, 43,
            "the mockup's 434.95 px leaves \(solved) px for each explained row, more than "
                + "three lines of sentence at 412 px wide: the model is missing a section")
        XCTAssertEqual(
            solved, mockup.rowDetailHeight, accuracy: 1,
            "the sentence height this file passes in is no longer the one the mockup's "
                + "measurement implies")
    }

    /// And the quieter arrangement scene 59 also measured, 434.95 px with no
    /// request waiting. A request must ADD height, by exactly one knock row.
    func testTheSamePaneWithNoRequestWaitingIsShorterByOneRequestRow() {
        let withRequest = PeerPaneLayout.height(
            PeerPaneLayout.Content(trustedMacs: 2, pendingKnocks: 1), metrics: mockup)
        let without = PeerPaneLayout.height(
            PeerPaneLayout.Content(trustedMacs: 2, pendingKnocks: 0), metrics: mockup)
        XCTAssertEqual(withRequest - without, mockup.knockRowHeight)
        XCTAssertEqual(
            without, measuredWithoutRequest, accuracy: 0.02 * measuredWithoutRequest,
            "scene 59's second measurement, the pane with nobody knocking")
        XCTAssertEqual(
            measuredWithRequest - measuredWithoutRequest, mockup.knockRowHeight, accuracy: 1,
            "the request row this file passes in is not the difference the mockup measured "
                + "between its two scenes, which is the only figure that difference can be")
    }

    /// **The redesign's whole claim.** Gil called the first pane "way too
    /// long, almost unusable"; the short one is the design. At one set of
    /// numbers, with two Macs trusted: the retired pane is more than twice the
    /// hole and the short one is inside a tenth of it.
    ///
    /// This is the test that would go red if the three
    /// per-Mac grant switches, or the six default-lease rows, went back on the
    /// top level, which is exactly how the first pane got long.
    func testTheShortPaneIsAFractionOfThePaneItReplaces() {
        let content = PeerPaneLayout.Content(trustedMacs: 2, pendingKnocks: 1)
        let short = PeerPaneLayout.height(content, metrics: mockup)
        let retired = PeerPaneLayout.retiredLongPaneHeight(trustedMacs: 2, metrics: mockup)
        XCTAssertGreaterThan(
            retired, 2 * PeerPaneLayout.availableHeight,
            "the pane being replaced has stopped being too long for its window, which means "
                + "this comparison is no longer measuring what it was written to measure")
        XCTAssertLessThan(short, retired / 2)
        XCTAssertLessThan(
            PeerPaneLayout.overflow(content, metrics: mockup),
            0.1 * PeerPaneLayout.availableHeight,
            "the short pane is supposed to sit inside its 500 pt hole, give or take the "
                + "difference between a browser's box model and a SwiftUI Form")
    }

    /// Scene 60: six trusted Macs scroll, and the LIST is the only thing that
    /// grew. Four more Macs cost exactly four compact rows, not four rows
    /// plus a re-measured section head, and not three switches each, which is
    /// what they cost before they moved into the sheet.
    func testFourMoreMacsCostExactlyFourCompactRows() {
        let two = PeerPaneLayout.Content(trustedMacs: 2, pendingKnocks: 0)
        let six = PeerPaneLayout.Content(trustedMacs: 6, pendingKnocks: 0)
        XCTAssertEqual(
            PeerPaneLayout.height(six, metrics: mockup)
                - PeerPaneLayout.height(two, metrics: mockup),
            4 * mockup.peerRowHeight)
        XCTAssertFalse(
            PeerPaneLayout.fits(six, metrics: mockup),
            "six Macs are meant to scroll, that is scene 60, and a pane that claimed to fit "
                + "at any peer count would be claiming its rows cost nothing")
    }

    /// Scene 61: open, the pane runs past the hole on purpose (865.13 px,
    /// 365 px past it) and this type says so rather than pretending it fits.
    /// That scroll is paid only by the operator who went looking.
    func testTheOpenAdvancedDisclosureOverflowsAndSaysSo() {
        let open = PeerPaneLayout.Content(
            trustedMacs: 2, pendingKnocks: 1, advancedOpen: true, advancedRows: 7)
        XCTAssertFalse(PeerPaneLayout.fits(open, metrics: mockup))
        XCTAssertGreaterThan(PeerPaneLayout.overflow(open, metrics: mockup), 0)
        XCTAssertGreaterThan(
            PeerPaneLayout.height(open, metrics: mockup),
            PeerPaneLayout.height(
                PeerPaneLayout.Content(trustedMacs: 2, pendingKnocks: 1), metrics: mockup))
    }

    /// Closed, the disclosure still costs its own row: it is the LAST thing on
    /// the pane and the point is that it is reachable without a gesture, so
    /// its height is inside the budget the fit is measured against.
    func testTheClosedDisclosureIsInsideTheBudget() {
        let content = PeerPaneLayout.Content(trustedMacs: 2, pendingKnocks: 1)
        let withDisclosure = PeerPaneLayout.height(content, metrics: mockup)
        var noDisclosure = mockup
        noDisclosure = PeerPaneLayout.Metrics(
            sectionHeadHeight: mockup.sectionHeadHeight, rowHeight: mockup.rowHeight,
            rowDetailHeight: mockup.rowDetailHeight, knockRowHeight: mockup.knockRowHeight,
            peerRowHeight: mockup.peerRowHeight, groupChrome: mockup.groupChrome,
            groupGap: mockup.groupGap, disclosureHeight: 0)
        XCTAssertEqual(
            withDisclosure - PeerPaneLayout.height(content, metrics: noDisclosure),
            mockup.disclosureHeight)
    }

    // MARK: - The DRAWN pane, against what the window shows

    /// The measured document height of the drawn pane at the gate's own scene
    ///, two trusted Macs and one pairing request, Advanced closed.
    ///
    ///     apps/macos/.build/debug/TcrBar --render-settings /tmp/peers
    ///     # peers: scrolled to 0 pt … (document 538 pt, viewport 540 pt,
    ///     #        resting -52 pt, pane estimate 537 pt)
    private let measuredDrawnPane: CGFloat = 538

    /// **The gate this fit check exists for.** The pane an operator opens fits the
    /// part of the window they can see, with nothing scrolling.
    ///
    /// `availableHeight` (500) is the MOCKUP's hole and the right target for
    /// the mockup's own parts. This one is the drawn document against
    /// ``PeerPaneLayout/visibleDocumentHeight``, 540 pt, the shipped window's
    /// 592 pt of content less the toolbar's 52 pt inset, both printed by the
    /// harness.
    func testTheDrawnPaneFitsWhatTheWindowActuallyShows() {
        let drawn = PeerPaneLayout.height(
            PeerPaneLayout.drawnContent(trustedMacs: 2, pendingKnocks: 1),
            metrics: PeerPaneLayout.drawnMetrics)
        XCTAssertLessThanOrEqual(
            drawn, PeerPaneLayout.visibleDocumentHeight,
            "the Peers pane is \(drawn) pt against the \(PeerPaneLayout.visibleDocumentHeight) "
                + "pt an operator sees without scrolling, so the Advanced row at the bottom is "
                + "below the fold again, which is the whole point of shrinking the pane")
    }

    /// And the arithmetic is the same pane AppKit drew, not a wish about it.
    ///
    /// This is the cross-check that keeps the gate above honest: a metric
    /// tuned to make the assertion pass would move the model away from the
    /// measurement, and the measurement is a figure the harness prints.
    func testTheArithmeticReproducesTheDrawnPaneHeight() {
        let drawn = PeerPaneLayout.height(
            PeerPaneLayout.drawnContent(trustedMacs: 2, pendingKnocks: 1),
            metrics: PeerPaneLayout.drawnMetrics)
        XCTAssertEqual(
            drawn, measuredDrawnPane, accuracy: 0.02 * measuredDrawnPane,
            "the model says \(drawn) pt and the render harness measured "
                + "\(measuredDrawnPane) pt for the same scene. Re-run "
                + "--render-settings: either a metric here is now a guess, or the pane "
                + "changed and this figure was not re-measured")
    }

    /// What this reduction actually removed, in points: the same scene was 675 pt
    /// when it started (`--render-settings`, 2026-09-18, before any of it).
    /// A fifth of that is the smallest saving that can still be called
    /// closing the gap.
    func testTheDrawnPaneIsFarShorterThanTheOneWaveFiveInherited() {
        let inherited: CGFloat = 675
        let drawn = PeerPaneLayout.height(
            PeerPaneLayout.drawnContent(trustedMacs: 2, pendingKnocks: 1),
            metrics: PeerPaneLayout.drawnMetrics)
        XCTAssertLessThan(drawn, 0.85 * inherited)
    }

    /// **Why the sentences moved into the section heads**, as a number rather
    /// than as a story: on the drawn pane a second line in a head is cheaper
    /// than a row's own caption, which is cheaper than a section footer. Put
    /// them back on the rows and the pane grows by more than its margin.
    func testASentenceInAHeadIsCheaperThanACaptionOnARow() {
        let m = PeerPaneLayout.drawnMetrics
        XCTAssertLessThan(m.sectionSentenceHeight, m.rowDetailHeight)
        let asHeads = PeerPaneLayout.height(
            PeerPaneLayout.drawnContent(trustedMacs: 2, pendingKnocks: 1), metrics: m)
        let asCaptions = PeerPaneLayout.height(
            PeerPaneLayout.Content(
                trustedMacs: 2, pendingKnocks: 1, explainedRows: 2, sectionSentences: 0),
            metrics: m)
        XCTAssertGreaterThan(asCaptions, asHeads)
        XCTAssertGreaterThan(
            asCaptions, PeerPaneLayout.visibleDocumentHeight,
            "two sentences back on their rows is still inside the window, so this pane has "
                + "room it did not have when the trade was measured, re-measure before "
                + "trusting either number")
    }

    /// The margin, stated rather than implied: **a third trusted Mac
    /// scrolls.** 2 Macs and a knock fit with 2 pt to spare, and one more row
    /// is 37 pt.
    ///
    /// This is not a defect (scene 60 says a longer list scrolls), but it is
    /// the honest envelope, and it is here so nobody reads "the pane fits" as
    /// "the pane always fits".
    func testTheFitIsTwoMacsAndAKnockAndTheNextRowScrolls() {
        let m = PeerPaneLayout.drawnMetrics
        func fits(macs: Int, knocks: Int) -> Bool {
            PeerPaneLayout.height(
                PeerPaneLayout.drawnContent(trustedMacs: macs, pendingKnocks: knocks),
                metrics: m) <= PeerPaneLayout.visibleDocumentHeight
        }
        XCTAssertTrue(fits(macs: 2, knocks: 1))
        XCTAssertTrue(fits(macs: 3, knocks: 0))
        XCTAssertFalse(fits(macs: 3, knocks: 1))
    }

    /// Nobody trusted yet is a real state, and the first one a new operator
    /// sees. It has to fit.
    func testTheEmptyPaneFits() {
        XCTAssertTrue(
            PeerPaneLayout.fits(
                PeerPaneLayout.Content(trustedMacs: 0, pendingKnocks: 0), metrics: mockup))
    }
}
