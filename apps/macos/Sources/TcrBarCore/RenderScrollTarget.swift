import CoreGraphics

/// Where `--render-settings` has to scroll a pane so a named control is in the
/// captured frame.
///
/// # The bug this replaces
///
/// `RenderSettings.scrollTop(for:)` was `tab == .peers ? 470 : 0`, a number
/// measured once against one pane, in a comment that says so ("470 pt is what
/// brings Paste a key, Regenerate… and the trusted-Mac rows into one frame").
/// The pane it was measured against is gone: the short pane moved four of those
/// controls behind a disclosure, so 470 now scrolls past the end of a pane that
/// fits, and the capture shows white. A harness whose aim is a constant goes
/// wrong silently every time the thing it aims at moves, and the PNG still
/// looks like a successful render.
///
/// So the harness names the ELEMENT it needs (an accessibility identifier the
/// pane sets) and this computes the offset from where that element actually is.
/// When the element is not found the answer is zero, the top of the pane,
/// which is an honest capture of the wrong thing rather than a blank one, and
/// the caller says so on stdout.
public enum RenderScrollTarget {
    /// The offset to scroll to, clamped to what the scroll view can reach.
    ///
    /// - `targetMinY` / `targetMaxY`: the element's vertical extent in the
    ///   scroll view's DOCUMENT coordinates, top-down (`minY` is its top edge,
    ///   measured from the top of the document).
    /// - `viewportHeight`: what the clip view shows.
    /// - `documentHeight`: the whole document.
    ///
    /// Three cases, and the third is the one a hard-coded number gets wrong:
    ///
    /// 1. The element is already inside the viewport at offset zero, answer
    ///    zero, because scrolling would move it for nothing.
    /// 2. It is below the fold, scroll so its BOTTOM lands at the bottom of
    ///    the viewport, with a margin, which keeps the rows above it in frame.
    ///    Aiming its top at the top instead would capture the element and
    ///    nothing of the pane it belongs to.
    /// 3. The document is shorter than the viewport, or the element is at the
    ///    very end, clamp to `documentHeight - viewportHeight`, never past
    ///    it. Past it, `NSScrollView` shows the bounce region: white.
    public static func offset(
        targetMinY: CGFloat, targetMaxY: CGFloat, viewportHeight: CGFloat,
        documentHeight: CGFloat, margin: CGFloat = 12
    ) -> CGFloat {
        let reachable = max(0, documentHeight - viewportHeight)
        guard reachable > 0 else { return 0 }
        if targetMaxY <= viewportHeight { return 0 }
        let wanted = targetMaxY + margin - viewportHeight
        // Never past the element's own TOP: that is the offset at which its
        // first line sits at the top of the frame, and scrolling further
        // captures the control with its label already above the fold. An
        // element taller than the viewport is the case this bites on, and
        // showing its start is the right half to show.
        return min(reachable, max(0, min(wanted, targetMinY)))
    }

    /// The clip view origin to scroll to: the pane's own RESTING origin, plus
    /// the offset above.
    ///
    /// # The defect this exists to stop
    ///
    /// A SwiftUI pane inside a `.fullSizeContentView` window sits in a scroll
    /// view with a TOP CONTENT INSET, the room the toolbar needs. At rest the
    /// clip view's bounds origin is therefore NEGATIVE (`-inset`), not zero.
    /// `scroll(to: NSPoint(x: 0, y: 0))` throws that inset away and drags the
    /// document up by it, which put the first section head, "This Mac", with
    /// its badge, behind the title bar in both appearances (measured
    /// 2026-09-18 on `/tmp/lan-p2p/settings/peers-dark.png`: the head is drawn
    /// at 20 pt, under a 28 pt title bar, and the first readable row is Name at
    /// 48 pt). The harness printed `scrolled to 0 pt` throughout, which is
    /// true and is exactly why the capture looked deliberate: the pane FIT, and
    /// the scroll that should have been a no-op was a 28 pt shift.
    ///
    /// So a zero offset has to mean "leave it where it rests", and every other
    /// offset is measured FROM there.
    public static func clipOrigin(restingY: CGFloat, offset: CGFloat) -> CGFloat {
        restingY + offset
    }
}
