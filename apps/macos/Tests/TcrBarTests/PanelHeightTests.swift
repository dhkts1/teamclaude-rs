import CoreGraphics
import XCTest

@testable import TcrBarCore

/// Finding 9: the panel's height budget.
///
/// `Tok.panelMaxHeight` (520) clamps the scrolling account LIST, and the panel
/// is `header + Hairline + list + Hairline + footer`. The spend line wraps
/// rather than truncates and has no bounded length, so on a fleet already at
/// the cap its second and third rendered lines were added on top of 520 and
/// pushed Quit and the three settings checkboxes off the bottom of the popover
/// — with no scroll region around them to recover with.
///
/// What is under test is the arithmetic that holds the panel's TOTAL height to
/// what it was before that line existed: the cap stays 520, and whatever the
/// header grew beyond one line comes out of the list.
final class PanelHeightTests: XCTestCase {

    /// The measured figures the panel actually runs on: 520pt cap, 8pt row
    /// spacing, and a spend line whose one-line height is ~14pt.
    private let cap: CGFloat = 520
    private let spacing: CGFloat = 8
    private let minimum: CGFloat = 120
    private let oneLine: CGFloat = 14

    private func budget(headerLineHeight: CGFloat) -> CGFloat {
        PanelHeight.listBudget(
            cap: cap, headerOverflow: headerLineHeight - oneLine, minimum: minimum)
    }

    func testAOneLineHeaderStillGetsTheWholeCap() {
        XCTAssertEqual(budget(headerLineHeight: oneLine), cap)
    }

    func testAWrappedHeaderSpendsTheListsBudgetRatherThanThePanelsHeight() {
        let twoLines = budget(headerLineHeight: oneLine * 2)
        let threeLines = budget(headerLineHeight: oneLine * 3)
        XCTAssertEqual(twoLines, cap - oneLine)
        XCTAssertEqual(threeLines, cap - oneLine * 2)

        // The claim the finding is about, stated as arithmetic: the panel is
        // header + list, and its total must not grow with the header.
        XCTAssertEqual(oneLine * 3 + threeLines, oneLine + cap)
        XCTAssertEqual(oneLine * 2 + twoLines, oneLine + cap)
    }

    func testAHeaderShorterThanOneLineNeverGrowsTheBudget() {
        XCTAssertEqual(
            budget(headerLineHeight: 0), cap,
            "an unmeasured header (the first frame, or no spend line at all) is not a credit")
    }

    func testAnAbsurdHeaderStillLeavesAScrollableList() {
        XCTAssertEqual(
            budget(headerLineHeight: 900), minimum,
            "a zero-height scroll view reads as a broken panel, not as a full one")
    }

    func testTheViewportSumsTheRowsAndClampsToTheBudget() {
        let short = PanelHeight.visibleRowsHeight(
            rowHeights: [100, 120], spacing: spacing, budget: cap)
        XCTAssertEqual(short, 228, "two rows and the one gap between them")

        let long = PanelHeight.visibleRowsHeight(
            rowHeights: Array(repeating: 118, count: 13), spacing: spacing,
            budget: budget(headerLineHeight: oneLine * 3))
        XCTAssertEqual(
            long, cap - oneLine * 2,
            "a thirteen-account fleet fills the budget the wrapped header left it")
    }

    func testTheFirstFrameRendersAtTheBudgetRatherThanAtZero() {
        XCTAssertEqual(
            PanelHeight.visibleRowsHeight(rowHeights: [], spacing: spacing, budget: 400), 400,
            "before SwiftUI reports a row height the panel must not collapse")
    }
}
