import CoreGraphics
import XCTest

@testable import TcrBarCore

/// The panel's height budget.
///
/// `PanelHeight.panelMaxHeight` (520) clamps the scrolling account LIST, and the panel is
/// `header + Hairline + list + Hairline + footer`. The spend line wraps rather
/// than truncates and has no bounded length, so on a fleet already at the cap
/// its second and third rendered lines were added on top of 520 and pushed Quit
/// and the three settings checkboxes off the bottom of the popover — with no
/// scroll region around them to recover with.
///
/// What is under test is the arithmetic that holds the panel's TOTAL height to
/// what it was before that line existed: the cap stays 520, and whatever the
/// header grew beyond one line comes out of the list.
///
/// The two CLAMPS below are READ from `PanelHeight`, never restated. They used
/// to be hand-copied — `cap = 520`, `minimum = 120` — out of `Tok`, which the
/// test target does not link, so a change to `Tok.panelMinListHeight` left all
/// five assertions green while the running panel clamped to a height nobody had
/// tested: this repo's "green suite, broken surface" failure exactly. `Tok`
/// reads them from here now.
///
/// The INPUTS are a different thing and are supplied by the test: a row's
/// height, a header's rendered line height, the gap between cards, a hairline's
/// thickness. Driving those with chosen values is what tests the arithmetic —
/// what mattered is that no assertion here restates a clamp the panel applies.
final class PanelHeightTests: XCTestCase {

    /// One rendered line of the spend line, as SwiftUI measures it at runtime
    /// (`UsageLineBaselineKey`). NOT a constant the panel runs on and not a
    /// claim about `Tok.secondaryDigitFont`'s line height — it is an INPUT to
    /// the arithmetic, so the tests below drive several values through it and
    /// assert the relationship rather than the number. The old `oneLine = 14`
    /// called itself a measured figure and proved an identity about `14`.
    private let someLineHeights: [CGFloat] = [11, 14, 17.5]

    /// `Tok.rowSpacing` and `Tok.hairlineWidth` as the panel passes them. Inputs
    /// to the sum, not clamps on it — stated here so the arithmetic has
    /// something to run on, and asserted as a RELATIONSHIP below rather than as
    /// these numbers.
    private let spacing: CGFloat = 8
    private let hairline: CGFloat = 0.5

    private func budget(headerLineHeight: CGFloat, oneLine: CGFloat) -> CGFloat {
        PanelHeight.listBudget(
            headerOverflow: PanelHeight.headerOverflow(
                lineHeight: headerLineHeight, oneLineHeight: oneLine, lineIsDrawn: true))
    }

    func testAOneLineHeaderStillGetsTheWholeCap() {
        for oneLine in someLineHeights {
            XCTAssertEqual(
                budget(headerLineHeight: oneLine, oneLine: oneLine), PanelHeight.panelMaxHeight,
                "whatever one line measures, one line spends nothing")
        }
    }

    func testAWrappedHeaderSpendsTheListsBudgetRatherThanThePanelsHeight() {
        for oneLine in someLineHeights {
            let twoLines = budget(headerLineHeight: oneLine * 2, oneLine: oneLine)
            let threeLines = budget(headerLineHeight: oneLine * 3, oneLine: oneLine)
            XCTAssertEqual(twoLines, PanelHeight.panelMaxHeight - oneLine)
            XCTAssertEqual(threeLines, PanelHeight.panelMaxHeight - oneLine * 2)

            // The claim the finding is about, stated as arithmetic: the panel is
            // header + list, and its total must not grow with the header.
            XCTAssertEqual(oneLine * 3 + threeLines, oneLine + PanelHeight.panelMaxHeight)
            XCTAssertEqual(oneLine * 2 + twoLines, oneLine + PanelHeight.panelMaxHeight)
        }
    }

    func testAHeaderShorterThanOneLineNeverGrowsTheBudget() {
        XCTAssertEqual(
            budget(headerLineHeight: 0, oneLine: 14), PanelHeight.panelMaxHeight,
            "an unmeasured header (the first frame, or no spend line at all) is not a credit")
    }

    func testAnAbsurdHeaderStillLeavesAScrollableList() {
        XCTAssertEqual(
            budget(headerLineHeight: 900, oneLine: 14), PanelHeight.panelMinListHeight,
            "a zero-height scroll view reads as a broken panel, not as a full one")
    }

    func testTheViewportSumsTheRowsAndClampsToTheBudget() {
        let short = PanelHeight.visibleRowsHeight(
            rowHeights: [100, 120], spacing: spacing, budget: PanelHeight.panelMaxHeight)
        XCTAssertEqual(short, 220 + spacing, "two rows and the one gap between them")

        let long = PanelHeight.visibleRowsHeight(
            rowHeights: Array(repeating: 118, count: 13),
            spacing: spacing,
            budget: budget(headerLineHeight: 42, oneLine: 14))
        XCTAssertEqual(
            long, PanelHeight.panelMaxHeight - 28,
            "a thirteen-account fleet fills the budget the wrapped header left it")
    }

    func testTheFirstFrameRendersAtTheBudgetRatherThanAtZero() {
        XCTAssertEqual(
            PanelHeight.visibleRowsHeight(rowHeights: [], spacing: spacing, budget: 400), 400,
            "before SwiftUI reports a row height the panel must not collapse")
    }

    /// Round two, finding 2. A fleet WITH a wrapped spend line and then WITHOUT
    /// one leaves the list at the full cap.
    ///
    /// Both measurements come from SwiftUI preferences, which are only emitted
    /// while the emitting view is on screen, so when the line stops rendering
    /// the last measured pair is just the last thing anyone said. That is not an
    /// exotic state — it is what the panel shows the moment the proxy is
    /// restarted onto a build predating `usage`, or the read goes offline. The
    /// old arithmetic went on subtracting a two-line header that was no longer
    /// there, clamping the account list one to two rows short for the rest of
    /// the session.
    func testAHeaderThatStopsDrawingItsSpendLineGivesTheBudgetBack() {
        let wrapped = PanelHeight.headerOverflow(
            lineHeight: 42, oneLineHeight: 14, lineIsDrawn: true)
        XCTAssertEqual(wrapped, 28, "three rendered lines cost the list two")
        XCTAssertEqual(PanelHeight.listBudget(headerOverflow: wrapped), PanelHeight.panelMaxHeight - 28)

        // The same stale pair, with the line no longer rendered.
        let gone = PanelHeight.headerOverflow(
            lineHeight: 42, oneLineHeight: 14, lineIsDrawn: false)
        XCTAssertEqual(gone, 0, "a header with no spend line has no overflow, whatever was measured")
        XCTAssertEqual(
            PanelHeight.listBudget(headerOverflow: gone), PanelHeight.panelMaxHeight,
            "the list gets the whole cap back")
    }

    /// Round two, finding 3. `accountList` puts a `Hairline` after index 0 when
    /// that row is the control account, which makes the inner VStack `n+1`
    /// children — `n` gaps rather than `n-1`, plus the hairline's own 0.5pt.
    /// `rowHeights` is filled from `AccountRow`'s GeometryReader alone, so
    /// neither term is in the array, and the viewport came out 8.5pt shorter
    /// than its content on the ordinary configuration for this panel: a control
    /// account pinned and a fleet under the cap. The bottom card was clipped
    /// mid-line and the list scrolled where it should have sat flush.
    func testAPinnedControlRowsSeparatorIsInTheSum() {
        let rows: [CGFloat] = [118, 118, 118]
        let without = PanelHeight.visibleRowsHeight(
            rowHeights: rows, spacing: spacing, budget: PanelHeight.panelMaxHeight)
        let with = PanelHeight.visibleRowsHeight(
            rowHeights: rows, spacing: spacing, controlHairline: hairline,
            budget: PanelHeight.panelMaxHeight)
        XCTAssertEqual(without, 354 + spacing * 2, "three cards and the two gaps between them")
        XCTAssertEqual(
            with, without + spacing + hairline,
            "the separator costs its own thickness AND one more gap")
        XCTAssertEqual(
            with - without, 8.5, accuracy: 0.0001,
            "which at the panel's own 8pt gap and 0.5pt hairline is 8.5pt of clipped card")
    }

    /// The separator is still bounded by the budget — a fleet at the cap does
    /// not grow the viewport past it to make room for a hairline.
    func testTheSeparatorNeverPushesTheViewportPastItsBudget() {
        let atCap = PanelHeight.visibleRowsHeight(
            rowHeights: Array(repeating: 118, count: 13),
            spacing: spacing,
            controlHairline: hairline,
            budget: PanelHeight.panelMaxHeight)
        XCTAssertEqual(atCap, PanelHeight.panelMaxHeight)
    }

    // MARK: - `quantized` — the runaway `NSPopover` layout fix

    func testQuantizedSnapsRawMeasurementsToTheGrain() {
        XCTAssertEqual(PanelHeight.quantized(41.998), 42.0)
        XCTAssertEqual(PanelHeight.quantized(42.24), 42.0)
        XCTAssertEqual(PanelHeight.quantized(42.26), 42.5)
    }

    /// The claim the fix rests on: a `GeometryReader` measurement that never
    /// repeats bit-for-bit across layout passes — the shape AppKit's own
    /// safe-area-inset recomputation produces on an `NSPopover` resize, and
    /// exactly what `onPreferenceChange` cannot tell apart from a row that is
    /// still genuinely growing — publishes the SAME quantized value once every
    /// raw measurement lands within half a grain of the others. That equality
    /// is what stops `onPreferenceChange` from re-firing, `@State` from
    /// re-assigning, and the layout pass it triggers from ever running again.
    ///
    /// Fails on the change this guards against: quantizing removed (return the
    /// raw measurement unchanged) leaves 20 distinct published values instead
    /// of one, which is the non-terminating case this arithmetic exists to
    /// close off.
    func testQuantizedStopsSubPixelJitterFromEverPublishingADifferentValue() {
        let jitter: [CGFloat] = (0..<20).map { 118.0 + CGFloat($0) * 0.00001 }
        XCTAssertEqual(Set(jitter).count, jitter.count, "the raw values really do all differ")

        let quantizedValues = Set(jitter.map(PanelHeight.quantized))
        XCTAssertEqual(
            quantizedValues, [118.0],
            "every measurement within the grain must publish the identical value")
    }

    /// Why ``PanelHeight/settled(_:_:)`` has to exist, stated as the failure it
    /// prevents.
    ///
    /// The fixture above centres its jitter on 118.0 — a grid POINT — and so it
    /// passes with quantizing alone and always would. Move the identical jitter
    /// onto a cell BOUNDARY and the same code publishes two values forever:
    /// half the samples round down and half round up. That is not damped noise,
    /// it is a manufactured two-state oscillation, and each alternation re-fires
    /// the observer and moves the list's frame half a point.
    ///
    /// The first assertion is this test's positive control. It is the shipped
    /// fixture's centring, so if it ever fails the harness is wrong rather than
    /// the code.
    func testQuantizingAloneRepublishesWhenJitterStraddlesACellBoundary() {
        func jitter(around centre: CGFloat) -> [CGFloat] {
            (0..<20).map { centre + (CGFloat($0) - 10) * 0.00001 }
        }
        XCTAssertEqual(
            Set(jitter(around: 118.0).map(PanelHeight.quantized)).count, 1,
            "positive control: on a grid point, quantizing alone already publishes one value")
        XCTAssertEqual(
            Set(jitter(around: 118.25).map(PanelHeight.quantized)).count, 2,
            "on a cell boundary the identical jitter publishes two — the limit cycle")
    }

    /// The dead band holds one value wherever the jitter is centred.
    ///
    /// Fails on the change this guards against: make ``PanelHeight/settled(_:_:)``
    /// return `quantized(measured)` unconditionally — today's shipped behaviour —
    /// and the two boundary centres publish two values each.
    func testSubGrainJitterPublishesOneValueWhereverItIsCentred() {
        for centre in [118.0, 118.25, 42.75] as [CGFloat] {
            let jitter: [CGFloat] = (0..<20).map { centre + (CGFloat($0) - 10) * 0.00001 }
            XCTAssertEqual(
                Set(jitter).count, jitter.count,
                "the raw values really do all differ, centre \(centre)")

            var previous = PanelHeight.quantized(jitter[0])
            var published: Set<CGFloat> = [previous]
            for raw in jitter {
                previous = PanelHeight.settled(previous, raw)
                published.insert(previous)
            }
            XCTAssertEqual(
                published.count, 1,
                "centre \(centre) published \(published.sorted()) — must publish exactly one")
        }
    }

    /// A real content change is larger than a grain, so the band never swallows
    /// it. Without this the fix would trade a crash for a panel that never
    /// resizes.
    func testARealChangeStillMoves() {
        XCTAssertEqual(PanelHeight.settled(118.0, 130.0), 130.0)
        XCTAssertEqual(PanelHeight.settled(118.0, 118.9), 119.0)
    }

    /// The per-row form holds each row independently, drops rows that left the
    /// fleet, and publishes a new row on the grid since it has nothing to hold to.
    func testPerRowSettlingHoldsDropsAndAdmits() {
        let previous: [String: CGFloat] = ["a": 40.0, "gone": 12.0]
        let measured: [String: CGFloat] = ["a": 40.2, "new": 17.26]
        let out = PanelHeight.settled(previous, measured)
        XCTAssertEqual(out["a"], 40.0, "within a grain of the last value — held")
        XCTAssertEqual(out["new"], 17.5, "no previous value — published on the grid")
        XCTAssertNil(out["gone"], "absent from the measurement — dropped, not held")
    }
}
