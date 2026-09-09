import CoreGraphics
import XCTest

@testable import TcrBarCore

/// The panel's AUTHORED size — the arithmetic `MenuBarShell` will hand to
/// `NSPopover.contentSize` in phase 3, and nothing calls yet.
///
/// What is under test is one property, and every assertion below is a
/// restatement of it: **a wrong chrome estimate lands on the account list and
/// never on the footer.** `PanelHeight.swift:6-19` records what the other
/// direction costs — a header that wrapped pushed Quit and the three settings
/// checkboxes off the bottom of the popover — and `docs/plans/panel-sizing-generalization.md`
/// §10 item 2 says the same thing as a rule: any design where a wrong header
/// estimate moves the footer re-creates that bug by a new route.
///
/// ``FakeTextMetrics`` is not a claim about TextKit. The whole point of
/// injecting `TextMetrics` is that the estimate is ALLOWED to be wrong, so a
/// test whose fake agreed with `NSString.boundingRect` would be testing the
/// wrong thing. What these tests drive through it is a relationship: whatever
/// number the text engine returns, this is where it lands.
///
/// The geometry is likewise an INPUT, supplied here, not read from `Tok` —
/// `Tok` lives in the executable target and this bundle links `TcrBarCore`
/// only (`Package.swift:39-43`). The two CLAMPS are the exception and are read
/// from `PanelHeight`, never restated, for the reason `PanelHeightTests`
/// gives: a hand-copied clamp leaves the assertions green while the running
/// panel clamps to a height nobody tested.
final class PanelSizeTests: XCTestCase {

    /// A text engine with no fonts in it: every character is `charWidth` wide
    /// and every rendered line is `lineHeight` tall, both scaled by the font
    /// size so a caller that forgets to forward the size is visible.
    ///
    /// Deterministic on purpose. `NSString.boundingRect` needs AppKit, and
    /// adding AppKit here would mean adding `"TcrBar"` to the test target —
    /// the one thing §10 item 5 forbids, and the reason `TextMetrics` is a
    /// protocol at all.
    private struct FakeTextMetrics: TextMetrics {
        let charWidth: CGFloat = 6
        let lineHeight: CGFloat = 12

        func height(of text: String, fontSize: CGFloat, width: CGFloat) -> CGFloat {
            let scale = fontSize / 12
            let perLine = max(1, Int(width / (charWidth * scale)))
            let lines = max(1, Int((Double(text.count) / Double(perLine)).rounded(.up)))
            return CGFloat(lines) * lineHeight * scale
        }
    }

    /// Records what it was asked, so a test can assert on the arguments rather
    /// than only on the sum. Answers a constant: the arguments are the subject.
    private final class RecordingTextMetrics: TextMetrics {
        struct Ask: Equatable {
            let text: String
            let fontSize: CGFloat
            let width: CGFloat
        }
        private(set) var asks: [Ask] = []

        func height(of text: String, fontSize: CGFloat, width: CGFloat) -> CGFloat {
            asks.append(Ask(text: text, fontSize: fontSize, width: width))
            return 12
        }
    }

    /// `Tok`'s figures as the panel passes them (`Tokens.swift:250, 281-283,
    /// 312`), plus two the panel measures rather than declares. Inputs to the
    /// arithmetic, not clamps on it — several tests vary them and assert the
    /// relationship instead of the number.
    private let geometry = PanelSize.Geometry(
        panelWidth: 380,
        gutter: 12,
        sectionSpacing: 8,
        lineSpacing: 4,
        rowSpacing: 8,
        hairlineWidth: 0.5,
        rowHeight: 42,
        fixedControlsHeight: 150
    )

    private let metrics = FakeTextMetrics()

    private func line(_ chars: Int, fontSize: CGFloat = 12) -> PanelSize.Line {
        PanelSize.Line(String(repeating: "x", count: chars), fontSize: fontSize)
    }

    private var someHeader: [PanelSize.Line] { [line(20), line(40)] }
    private var someFooter: [PanelSize.Line] { [line(30)] }

    private func plan(
        rows: Int,
        header: [PanelSize.Line]? = nil,
        footer: [PanelSize.Line]? = nil,
        geometry: PanelSize.Geometry? = nil
    ) -> PanelSize.Plan {
        PanelSize.plan(
            rowCount: rows,
            header: header ?? someHeader,
            footer: footer ?? someFooter,
            geometry: geometry ?? self.geometry,
            metrics: metrics
        )
    }

    // MARK: - the invariant

    /// The whole design in one assertion. A header that grows from one
    /// rendered line to five must not shorten the footer by a single point:
    /// the panel gets taller instead, which is what "author the container"
    /// means. Under the shipped `.preferredContentSize` path the popover's
    /// height was capped and the growth came out of whatever was last in the
    /// stack — the footer — which is `PanelHeight.swift:6-19`'s bug.
    func testTheFooterKeepsItsFullHeightHoweverTheHeaderWraps() {
        let short = plan(rows: 6, header: [line(10)])
        let long = plan(rows: 6, header: [line(10_000)])

        XCTAssertGreaterThan(
            long.headerHeight, short.headerHeight * 5,
            "the fake really does wrap a 10,000-character line onto many more lines")
        XCTAssertEqual(
            long.footerHeight, short.footerHeight,
            "a wrapping header may not cost the footer one point")
        XCTAssertEqual(
            long.height - short.height, long.headerHeight - short.headerHeight,
            accuracy: 0.0001,
            "the panel absorbs the header's growth by getting taller, nothing else")
    }

    /// The other half: the list is a function of the fleet, so chrome the
    /// estimate got wrong is not charged to it either. `headerOverflow`
    /// (`PanelHeight.swift:75`) charged exactly this, and phase 5 deletes it —
    /// under an authored container with a residual scroll region the list
    /// absorbs the error by scrolling a little early, which costs nothing that
    /// needs paying for in advance.
    func testAWrappingHeaderTakesNothingFromTheList() {
        XCTAssertEqual(
            plan(rows: 6, header: [line(10_000)]).listHeight,
            plan(rows: 6, header: [line(10)]).listHeight,
            "the list's height is a fact about the fleet, not about the header")
    }

    // MARK: - the list estimate

    /// Biased DOWN by one row and its gap, deliberately. The estimate is going
    /// to be wrong in one direction or the other; too short scrolls slightly
    /// early, too tall leaves a void under the last row that nothing can fill.
    /// Scrolling early is the cheaper mistake, so the bias is one whole row
    /// rather than a fudge factor — a unit a reader can name.
    func testTheListEstimateIsBiasedDownByOneRow() {
        let rows = 6
        let natural =
            CGFloat(rows) * geometry.rowHeight + CGFloat(rows - 1) * geometry.rowSpacing

        XCTAssertEqual(
            PanelSize.listHeight(rowCount: rows, geometry: geometry),
            natural - geometry.rowHeight - geometry.rowSpacing,
            accuracy: 0.0001,
            "one row and one gap short of the content, so the list scrolls early rather than gapping")
    }

    /// The floor is one row, NOT `panelMinListHeight`. 120 is the floor on the
    /// old list BUDGET — the space a long header may not take away — and
    /// `docs/plans/panel-sizing-generalization.md` §8 is explicit that a fleet
    /// which really has one row should draw 42pt and not 120. Clamping up to
    /// the budget here would draw 78pt of empty panel under a single account.
    func testAOneRowFleetGetsOneRowOfListNotTheOldMinimumBudget() {
        XCTAssertEqual(
            PanelSize.listHeight(rowCount: 1, geometry: geometry), geometry.rowHeight,
            "one account draws one row of list")
        XCTAssertLessThan(
            PanelSize.listHeight(rowCount: 1, geometry: geometry),
            PanelHeight.panelMinListHeight,
            "and is allowed to be shorter than the budget floor, which is a different figure")
    }

    /// `panelMaxHeight` caps the LIST and not the panel — its own doc-comment
    /// says so (`PanelHeight.swift:44-50`), and capping the panel instead is
    /// how the footer went off the bottom. A 40-account fleet scrolls; its
    /// chrome is still authored in full on top of the cap.
    func testTheListIsCappedAtPanelMaxHeightAndTheChromeIsNot() {
        let huge = plan(rows: 40)

        XCTAssertEqual(
            huge.listHeight, PanelHeight.panelMaxHeight,
            "a fleet past the cap gets the cap and scrolls")
        XCTAssertEqual(
            huge.height, PanelHeight.panelMaxHeight + huge.chromeHeight,
            accuracy: 0.0001,
            "the cap is on the list; the chrome is added on top of it, never squeezed into it")
        XCTAssertGreaterThan(
            huge.height, PanelHeight.panelMaxHeight,
            "so the authored PANEL is taller than the cap — capping the total is the bug")
    }

    func testAnEmptyFleetGetsNoList() {
        XCTAssertEqual(
            PanelSize.listHeight(rowCount: 0, geometry: geometry), 0,
            "no accounts, no scroll region — `FleetView.content` draws a banner instead")
    }

    // MARK: - what the chrome reserves

    /// Every structural piece of `FleetView.body` (`FleetView.swift:93-103`)
    /// and nothing else: `.padding(Tok.gutter)` top and bottom, the two
    /// `Hairline()`s, and the four gaps between its five children. Restated
    /// here because it is a claim ABOUT that view: if a child is added or a
    /// hairline removed and this number is not updated, the authored size is
    /// wrong by a gap and the last thing in the panel hangs over the edge.
    func testTheFrameReservesBothGuttersBothHairlinesAndEveryGap() {
        XCTAssertEqual(
            plan(rows: 3).frameHeight,
            2 * geometry.gutter + 2 * geometry.hairlineWidth
                + CGFloat(PanelSize.bodyChildren - 1) * geometry.sectionSpacing,
            accuracy: 0.0001)
    }

    /// The height is the sum of its four named parts and nothing hidden. This
    /// is what makes phase 3's log line readable: a delta against the popover's
    /// real `contentSize` can be attributed to header, footer or list.
    func testTheHeightIsExactlyItsFourParts() {
        let p = plan(rows: 5)
        XCTAssertEqual(
            p.height, p.headerHeight + p.footerHeight + p.frameHeight + p.listHeight,
            accuracy: 0.0001)
        for part in [p.headerHeight, p.footerHeight, p.frameHeight, p.listHeight] {
            XCTAssertGreaterThan(
                part, 0,
                "and every part carries weight — the identity above holds trivially if one is zero")
        }
    }

    /// Text is measured at the width it will actually be drawn at — the panel
    /// less both gutters — and the caller's font size is forwarded rather than
    /// assumed. Measuring at `panelWidth` under-counts the wrapped lines of
    /// every string in the panel, which is the estimate being wrong in the one
    /// direction that puts the footer over the edge.
    func testTextIsMeasuredAtThePanelWidthLessBothGuttersInTheCallersFont() {
        let recorder = RecordingTextMetrics()
        _ = PanelSize.plan(
            rowCount: 3,
            header: [PanelSize.Line("spend", fontSize: 11)],
            footer: [PanelSize.Line("server abc1234", fontSize: 10)],
            geometry: geometry,
            metrics: recorder
        )

        XCTAssertEqual(
            recorder.asks,
            [
                .init(text: "spend", fontSize: 11, width: geometry.panelWidth - 2 * geometry.gutter),
                .init(
                    text: "server abc1234", fontSize: 10,
                    width: geometry.panelWidth - 2 * geometry.gutter),
            ],
            "every chrome string, at the drawn width, in its own font size")
    }

    /// The lines of one stack are separated by `Tok.tightSpacing`, the spacing
    /// `header` and `footer` both give their `VStack` (`FleetView.swift:109`,
    /// `:450`). Two lines is one gap, not two and not none.
    func testAStackOfLinesReservesTheGapsBetweenThem() {
        let one = PanelSize.stackHeight([line(5)], geometry: geometry, metrics: metrics)
        let three = PanelSize.stackHeight(
            [line(5), line(5), line(5)], geometry: geometry, metrics: metrics)

        XCTAssertEqual(three, 3 * one + 2 * geometry.lineSpacing, accuracy: 0.0001)
        XCTAssertEqual(
            PanelSize.stackHeight([], geometry: geometry, metrics: metrics), 0,
            "a stack that draws nothing reserves nothing, gap included")
    }

    /// The footer's fixed controls — the buttons and the three checkboxes —
    /// are reserved whether or not the footer draws any wrapping text, and the
    /// gap between text and controls only exists when both do.
    func testTheFooterReservesItsFixedControlsWithOrWithoutText() {
        let withText = plan(rows: 3, footer: [line(30)])
        let textless = plan(rows: 3, footer: [])

        XCTAssertEqual(
            textless.footerHeight, geometry.fixedControlsHeight,
            "no footer text still reserves Quit and the checkboxes")
        XCTAssertEqual(
            withText.footerHeight,
            geometry.fixedControlsHeight + geometry.lineSpacing
                + PanelSize.stackHeight([line(30)], geometry: geometry, metrics: metrics),
            accuracy: 0.0001,
            "and with text, one gap between the text and the controls")
    }

    // MARK: - from a fleet

    /// `forFleet` counts the rows the panel will draw. Decoded from JSON
    /// rather than built with the memberwise init, so the fixture is the shape
    /// `tcr status --json` really emits.
    func testForFleetSizesTheListFromTheAccountCount() throws {
        let fleet = try Fleet.decode(Data(fleetJSON(rows: 7).utf8))
        XCTAssertEqual(fleet.accounts.count, 7, "the fixture decoded")

        let size = PanelSize.forFleet(
            fleet, header: someHeader, footer: someFooter, geometry: geometry, metrics: metrics)

        XCTAssertEqual(size, plan(rows: 7).contentSize)
        XCTAssertEqual(size.width, geometry.panelWidth, "the panel's width is authored, not measured")
    }

    private func fleetJSON(rows: Int) -> String {
        let accounts = (0..<rows).map { index in
            """
            {"name":"account\(index)@example.com","priority":\(index),"status":"active",
             "disabled":false,"quota":0.5,"quotaState":"ok","fiveHour":0.5,"sevenDay":0.5,
             "sevenDayOi":null,"held":[],"requests":1,"inputTokens":1,"outputTokens":1,
             "cacheReadTokens":1,"cacheHitRatio":0.5,"probeStatus":"ok","probeError":null,
             "lastStreamError":null,"streamErrorCount":0,"source":"live",
             "serverSha":"abc1234","serverDirty":false}
            """
        }
        return "[\(accounts.joined(separator: ","))]"
    }
}
