import XCTest

@testable import TcrBarCore

/// Where `--render-settings` scrolls, now that it aims at an ELEMENT instead of
/// the constant 470 pt.
///
/// The bug the constant had: it was measured once, against a pane that no
/// longer exists. Every case below is one way a fixed number goes wrong while
/// the PNG still looks like a successful render.
final class RenderScrollTargetTests: XCTestCase {

    /// Already in frame: no scroll. A fixed 470 would have moved it out.
    func testAnElementAlreadyInTheViewportIsNotScrolledTo() {
        XCTAssertEqual(
            RenderScrollTarget.offset(
                targetMinY: 300, targetMaxY: 330, viewportHeight: 500, documentHeight: 900),
            0)
    }

    /// Below the fold: scroll just enough to land its bottom inside the frame,
    /// which keeps the pane above it in the picture. Aiming its top at the top
    /// of the viewport instead would capture the control and none of what it
    /// belongs to.
    func testAnElementBelowTheFoldIsBroughtToTheBottomOfTheFrame() {
        XCTAssertEqual(
            RenderScrollTarget.offset(
                targetMinY: 800, targetMaxY: 830, viewportHeight: 500, documentHeight: 1200,
                margin: 12),
            342)
    }

    /// **The case the constant gets wrong.** A pane that FITS has nothing to
    /// scroll, and 470 against a 493 pt document in a 500 pt viewport scrolls
    /// into the bounce region: the capture comes back white and the render
    /// still reports success.
    func testAPaneThatFitsIsNeverScrolledIntoTheBounceRegion() {
        XCTAssertEqual(
            RenderScrollTarget.offset(
                targetMinY: 460, targetMaxY: 493, viewportHeight: 500, documentHeight: 493),
            0)
    }

    /// Never past what the scroll view can reach, even when the element's own
    /// position asks for more.
    func testTheOffsetIsClampedToWhatTheScrollViewCanReach() {
        XCTAssertEqual(
            RenderScrollTarget.offset(
                targetMinY: 1150, targetMaxY: 1200, viewportHeight: 500, documentHeight: 1200),
            700)
    }

    /// An element taller than the viewport shows its START rather than its
    /// end: scrolling to put its bottom in frame would push its own label
    /// above the fold, which is the half that says what it is.
    func testAnElementTallerThanTheViewportShowsItsTop() {
        XCTAssertEqual(
            RenderScrollTarget.offset(
                targetMinY: 600, targetMaxY: 1400, viewportHeight: 500, documentHeight: 2000),
            600)
    }

    /// A document shorter than its viewport has no reachable offset at all.
    func testAShortDocumentHasNoOffset() {
        XCTAssertEqual(
            RenderScrollTarget.offset(
                targetMinY: 100, targetMaxY: 140, viewportHeight: 500, documentHeight: 200),
            0)
    }

    // MARK: - A zero offset must LEAVE THE PANE WHERE IT RESTS

    /// **The defect this closes.** A pane that fits is scrolled to offset
    /// zero, and the harness used to hand AppKit a literal zero, which throws
    /// away the clip view's resting origin. In a `.fullSizeContentView` window
    /// that origin is `-52 pt` (the toolbar's inset, measured), so the "no-op"
    /// scroll dragged the document up by 52 pt and put the first section head
    /// behind the title bar in both appearances.
    ///
    /// Zero has to mean "don't move it".
    func testAZeroOffsetLeavesThePaneAtItsRestingOrigin() {
        XCTAssertEqual(RenderScrollTarget.clipOrigin(restingY: -52, offset: 0), -52)
    }

    /// And a real offset is measured FROM that origin, not from zero, 31 pt
    /// of scroll in a window whose content rests at -52 is -21, and handing
    /// AppKit 31 instead would lose the inset a second time.
    func testAnOffsetIsMeasuredFromTheRestingOrigin() {
        XCTAssertEqual(RenderScrollTarget.clipOrigin(restingY: -52, offset: 31), -21)
    }

    /// A window with no inset at all (no toolbar, resting at zero) is
    /// unaffected: the offset is the origin. This is the case that makes the
    /// fix invisible on four of the five panes, and the reason the defect
    /// only ever showed up on Peers.
    func testAPaneWithNoInsetIsUnchanged() {
        XCTAssertEqual(RenderScrollTarget.clipOrigin(restingY: 0, offset: 120), 120)
    }
}
