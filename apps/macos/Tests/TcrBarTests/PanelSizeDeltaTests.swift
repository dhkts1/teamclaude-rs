import CoreGraphics
import XCTest

@testable import TcrBarCore

/// The arithmetic behind phase 3's log line — the instrument that answers the
/// question the whole of `docs/plans/panel-sizing-generalization.md` rests on:
/// does the TextKit estimate track what SwiftUI actually lays out.
///
/// The shell does the I/O and this does the maths, which is the same split
/// `PanelHeight`, `PanelSize` and `UncaughtExceptionReport` already have. What
/// that buys is exactly this file: the log line a person on another Mac will
/// read once, asserted here rather than eyeballed there.
///
/// Two properties are under test, and every assertion is a restatement of one
/// of them.
///
/// 1. **The line names the parts, not only the total.** "wrong by 40pt" cannot
///    say whether the header, the footer or the list was wrong, and telling
///    those apart is the entire point of the phase.
/// 2. **An absent measurement reads as absent.** A popover that has never been
///    laid out reports a `contentSize` of zero, and subtracting a prediction
///    from it yields a confident −500pt that looks exactly like a catastrophic
///    mis-estimate. The one reader of these lines gets one look; a fabricated
///    delta is worse than a gap.
final class PanelSizeDeltaTests: XCTestCase {

    /// A prediction with four parts that are all different from each other, so
    /// an assertion cannot pass by reading the wrong field.
    private let plan = PanelSize.Plan(
        width: 380,
        headerHeight: 88,
        footerHeight: 176,
        listHeight: 228,
        frameHeight: 41
    )

    private func delta(
        actual: CGSize,
        rowCount: Int = 13,
        headerLines: Int = 4,
        footerLines: Int = 2,
        shown: Bool = true,
        predicted: PanelSize.Plan? = nil
    ) -> PanelSizeDelta {
        PanelSizeDelta(
            predicted: predicted ?? plan,
            actualContentSize: actual,
            rowCount: rowCount,
            headerLines: headerLines,
            footerLines: footerLines,
            isPanelShown: shown
        )
    }

    // MARK: - The delta itself

    /// `plan.height` is 88 + 176 + 41 + 228 = 533.
    func testHeightDeltaIsWhatSwiftUIProducedMinusWhatWePredicted() {
        XCTAssertEqual(delta(actual: CGSize(width: 380, height: 574.5)).heightDelta, 41.5)
    }

    /// The sign is the finding. An over-prediction leaves a void under the last
    /// row that no scroll region recovers; an under-prediction scrolls early.
    /// A magnitude-only delta cannot tell a reader which mistake was made.
    func testAnOverPredictionKeepsItsNegativeSign() {
        XCTAssertEqual(delta(actual: CGSize(width: 380, height: 520.5)).heightDelta, -12.5)
    }

    /// The panel has been a fixed-width column since it was written
    /// (`PanelSize.Geometry.panelWidth`), so a non-zero width delta means the
    /// popover is not honouring the authored width at all — a different fault
    /// from a height mis-estimate, and invisible if only the height is logged.
    func testWidthDeltaIsReportedSeparatelyFromHeight() {
        XCTAssertEqual(delta(actual: CGSize(width: 392, height: 533)).widthDelta, 12)
        XCTAssertEqual(delta(actual: CGSize(width: 392, height: 533)).heightDelta, 0)
    }

    /// The height the panel really had left over after the chrome we predicted
    /// — 574.5 − (88 + 176 + 41) = 269.5 against a predicted list of 228. This
    /// is the number phase 4's residual scroll region will absorb, so it is the
    /// one worth naming rather than leaving the reader to subtract.
    func testImpliedListHeightIsTheActualTotalLessThePredictedChrome() {
        XCTAssertEqual(
            delta(actual: CGSize(width: 380, height: 574.5)).impliedListHeight, 269.5)
    }

    // MARK: - An absent measurement

    /// An `NSPopover` that has never been laid out reports a zero
    /// `contentSize`. Treating that as a measurement produces `-533.0` — a
    /// number that reads as a catastrophic mis-estimate and is in fact no
    /// measurement at all.
    func testAZeroContentSizeIsNoMeasurementRatherThanAHugeNegativeDelta() {
        let unlaid = delta(actual: .zero, shown: false)
        XCTAssertNil(unlaid.actual)
        XCTAssertNil(unlaid.heightDelta)
        XCTAssertNil(unlaid.widthDelta)
        XCTAssertNil(unlaid.impliedListHeight)
    }

    /// Half a size is still not a size. A popover mid-configuration can report
    /// a width with no height, and the height is the entire subject here.
    func testAZeroHeightWithANonZeroWidthIsAlsoNoMeasurement() {
        XCTAssertNil(delta(actual: CGSize(width: 380, height: 0)).actual)
    }

    func testAnUnlaidPanelSaysSoInWordsInsteadOfPrintingANumber() {
        let line = delta(actual: .zero, shown: false).logLine
        XCTAssertTrue(line.contains("actual=unlaid"), line)
        XCTAssertTrue(line.contains("delta-height=unknown"), line)
        XCTAssertTrue(line.contains("delta-width=unknown"), line)
        XCTAssertTrue(line.contains("implied-list=unknown"), line)
        XCTAssertFalse(line.contains("-533"), line)
    }

    // MARK: - The line a person reads once

    /// One fixed marker at the head of the line, so a single
    /// `log show --predicate 'eventMessage CONTAINS "TCRBAR-PANELSIZE"'` finds
    /// every line and nothing else. The version is there because the field set
    /// is going to change between phases and a reader must not have to guess
    /// which shape they are holding.
    func testTheLineStartsWithTheMarkerAndAVersion() {
        XCTAssertTrue(
            delta(actual: CGSize(width: 380, height: 574.5)).logLine
                .hasPrefix("TCRBAR-PANELSIZE v1 "))
    }

    /// One line, or `log show` splits one observation across rows and the
    /// key=value grep stops working.
    func testTheLineIsOneLine() {
        XCTAssertFalse(delta(actual: CGSize(width: 380, height: 574.5)).logLine.contains("\n"))
    }

    /// Property 1, stated directly: every part of the prediction is on the
    /// line. A total alone cannot say which part was wrong.
    func testTheLineBreaksThePredictionDownByPart() {
        let line = delta(actual: CGSize(width: 380, height: 574.5)).logLine
        XCTAssertTrue(line.contains("header=88.0/4ln"), line)
        XCTAssertTrue(line.contains("footer=176.0/2ln"), line)
        XCTAssertTrue(line.contains("list=228.0"), line)
        XCTAssertTrue(line.contains("frame=41.0"), line)
        XCTAssertTrue(line.contains("predicted=380.0x533.0"), line)
        XCTAssertTrue(line.contains("actual=380.0x574.5"), line)
        XCTAssertTrue(line.contains("delta-height=+41.5"), line)
        XCTAssertTrue(line.contains("delta-width=+0.0"), line)
        XCTAssertTrue(line.contains("implied-list=269.5"), line)
    }

    /// The chrome line counts are what let a reader separate a wrapping
    /// header from a mis-sized footer across two ticks: `FleetView`'s header
    /// lines are all conditional, so the count moves on its own as the fleet
    /// changes state.
    func testTheLineCarriesTheRowAndChromeLineCounts() {
        let line = delta(
            actual: CGSize(width: 380, height: 574.5), rowCount: 9,
            headerLines: 3, footerLines: 1
        ).logLine
        XCTAssertTrue(line.contains("rows=9"), line)
        XCTAssertTrue(line.contains("/3ln"), line)
        XCTAssertTrue(line.contains("/1ln"), line)
    }

    /// The tick that measures the chrome estimate ALONE. With no accounts the
    /// list term is zero by construction, so the whole delta on such a line is
    /// header-and-footer error with nothing else mixed into it — which is the
    /// only single-tick attribution this instrument can honestly make.
    func testAnEmptyFleetLineCarriesAZeroListTermSoTheDeltaIsPureChrome() {
        let empty = PanelSize.Plan(
            width: 380, headerHeight: 44, footerHeight: 176, listHeight: 0, frameHeight: 41)
        let line = delta(
            actual: CGSize(width: 380, height: 268), rowCount: 0, headerLines: 2,
            footerLines: 1, predicted: empty
        ).logLine
        XCTAssertTrue(line.contains("rows=0"), line)
        XCTAssertTrue(line.contains("list=0.0"), line)
        XCTAssertTrue(line.contains("delta-height=+7.0"), line)
    }

    /// A `contentSize` read while the popover is closed is last-open state, not
    /// a fresh layout. The reader must be able to drop those lines.
    func testTheLineSaysWhetherThePanelWasOnScreen() {
        XCTAssertTrue(
            delta(actual: CGSize(width: 380, height: 574.5), shown: true).logLine
                .contains("shown=yes"))
        XCTAssertTrue(
            delta(actual: CGSize(width: 380, height: 574.5), shown: false).logLine
                .contains("shown=no"))
    }
}
