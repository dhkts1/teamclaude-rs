import CoreGraphics

/// The panel's height budget, as arithmetic a test can run.
///
/// The popover sizes itself to its content, so the panel's total height is
/// `header + Hairline + list + Hairline + footer` (`FleetView.body`). Only the
/// LIST was ever clamped — `Tok.panelMaxHeight`, documented as "what keeps Quit
/// and the checkboxes on screen under a long fleet" — so anything the header
/// grew was added on top of that cap unconditionally. The spend line wraps
/// rather than truncates (`.fixedSize(horizontal: false, vertical: true)`) and
/// has no bounded length: three models plus unpriced traffic runs to roughly 85
/// characters, two or three rendered lines in a 380pt panel, and on a fleet
/// already at the cap the Quit button and the settings checkboxes went off the
/// bottom of the popover with no scroll region around them to recover with.
///
/// The fix is not a smaller cap: `panelMaxHeight` stays 520 and a one-line
/// header still gets the full list. What the header takes BEYOND its one-line
/// baseline comes out of the list's budget instead, so the panel's total height
/// is what it was before the usage line existed.
///
/// Lives here rather than in `FleetView` because it is arithmetic, and the test
/// target links `TcrBarCore` only — a view-private helper could not be tested,
/// and this is the rule a wrapped header silently broke once already.
public enum PanelHeight {
    /// What the scrolling list may occupy: the cap, less whatever the header
    /// grew past one line.
    ///
    /// `overflow` is `measured header line height − one-line height`, never
    /// negative. Never returns less than `minimum`: a header long enough to eat
    /// the whole budget must still leave a list a reader can scroll, and a
    /// zero-height scroll view reads as a broken panel rather than a full one.
    public static func listBudget(
        cap: CGFloat,
        headerOverflow: CGFloat,
        minimum: CGFloat
    ) -> CGFloat {
        max(minimum, cap - max(0, headerOverflow))
    }

    /// The height to give the scroll viewport: every row plus the gaps between
    /// them, clamped to ``listBudget(cap:headerOverflow:minimum:)``.
    ///
    /// `[]` — the first frame, before SwiftUI has reported any row height —
    /// returns the budget itself, so the panel never renders at zero or
    /// one-row height while waiting for a real measurement.
    public static func visibleRowsHeight(
        rowHeights: [CGFloat],
        spacing: CGFloat,
        budget: CGFloat
    ) -> CGFloat {
        guard !rowHeights.isEmpty else { return budget }
        let summed = rowHeights.reduce(0, +) + spacing * CGFloat(max(rowHeights.count - 1, 0))
        return min(max(summed, spacing), budget)
    }
}
