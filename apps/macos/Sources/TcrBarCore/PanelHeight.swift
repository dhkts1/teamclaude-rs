import CoreGraphics

/// The panel's height budget, as arithmetic a test can run.
///
/// The popover sizes itself to its content, so the panel's total height is
/// `header + Hairline + list + Hairline + footer` (`FleetView.body`). Only the
/// LIST was ever clamped — ``cap``, documented as "what keeps Quit and the
/// checkboxes on screen under a long fleet" — so anything the header grew was
/// added on top of that cap unconditionally. The spend line wraps rather than
/// truncates (`.fixedSize(horizontal: false, vertical: true)`) and has no
/// bounded length: three models plus unpriced traffic runs to roughly 85
/// characters, two or three rendered lines in a 380pt panel, and on a fleet
/// already at the cap the Quit button and the settings checkboxes went off the
/// bottom of the popover with no scroll region around them to recover with.
///
/// The fix is not a smaller cap: ``cap`` stays 520 and a one-line header still
/// gets the full list. What the header takes BEYOND its one-line baseline comes
/// out of the list's budget instead, so the panel's total height is what it was
/// before the usage line existed.
///
/// Lives here rather than in `FleetView` because it is arithmetic, and the test
/// target links `TcrBarCore` only — a view-private helper could not be tested,
/// and this is the rule a wrapped header silently broke once already.
///
/// The two CLAMPS it runs on are authored here for the same reason, one step
/// further: the gate used to hand-copy them out of `Tok` (which is in the
/// executable target, unreachable from the tests), so a change to
/// `Tok.panelMinListHeight` left every assertion green while the running panel
/// clamped to a height nobody had tested. `Tok` reads these now, rather than
/// the other way round.
///
/// They keep the names `Tok` gave them, and `scripts/tcrbar-palette.py` reads
/// this file alongside `Tokens.swift`, because both are published design tokens
/// (`--tcr-panel-max-height`, `--tcr-panel-min-list-height`). Renaming them, or
/// moving them somewhere the generator does not look, deletes two tokens from
/// `design-tokens/` without deleting anything a reader would notice.
///
/// The two GEOMETRY figures the viewport sum also needs — the gap between cards
/// and a hairline's thickness — stay in `Tok` and arrive as parameters. They are
/// inputs to this arithmetic rather than clamps on it, so a test that drives
/// them with its own values tests the whole rule; the two above are different,
/// because an assertion about them IS an assertion about the running panel.
public enum PanelHeight {
    /// The cap on the scrolling account list — what keeps Quit and the
    /// checkboxes on screen under a long fleet. `Tok.panelMaxHeight`.
    ///
    /// A row count (`visibleAccountRows = 4`) used to do this and was the wrong
    /// unit: rows are not uniform height, so four of them is not a fixed number
    /// of points. It is the cap on the LIST, not on the panel: a header that
    /// wraps does not shrink this number, it spends part of it.
    public static let panelMaxHeight: CGFloat = 520

    /// The floor under that budget: a header long enough to eat the whole cap
    /// must still leave a list a reader can scroll. A zero-height scroll view
    /// reads as a broken panel, not as a full one. `Tok.panelMinListHeight`.
    public static let panelMinListHeight: CGFloat = 120

    /// What the scrolling list may occupy: the cap, less whatever the header
    /// grew past one line.
    ///
    /// `overflow` is `measured header line height − one-line height`, never
    /// negative. Never returns less than `minimum`: a header long enough to eat
    /// the whole budget must still leave a list a reader can scroll, and a
    /// zero-height scroll view reads as a broken panel rather than a full one.
    public static func listBudget(
        cap: CGFloat = panelMaxHeight,
        headerOverflow: CGFloat,
        minimum: CGFloat = panelMinListHeight
    ) -> CGFloat {
        max(minimum, cap - max(0, headerOverflow))
    }

    /// The height to give the scroll viewport: every row plus the gaps between
    /// them, clamped to ``listBudget(cap:headerOverflow:minimum:)``.
    ///
    /// `controlHairline` is the THICKNESS of the separator
    /// `FleetView.accountList` draws under a pinned control row, and `nil` when
    /// there is no such row — only the caller knows which, and only the caller
    /// knows `Tok.hairlineWidth`. Without this term the viewport came out
    /// `spacing + hairlineWidth` (8.5pt) shorter than its own content on the
    /// ordinary configuration for this panel — a control account pinned and a
    /// fleet under the cap — so the bottom card was clipped mid-line and the
    /// list scrolled where it should have sat flush. `rowHeights` cannot see
    /// it: that array is populated from `AccountRow`'s own GeometryReader, and
    /// the hairline is not an `AccountRow`. It costs its own thickness AND one
    /// more gap, because inserting it makes the VStack `n+1` children.
    ///
    /// `[]` — the first frame, before SwiftUI has reported any row height —
    /// returns the budget itself, so the panel never renders at zero or
    /// one-row height while waiting for a real measurement.
    public static func visibleRowsHeight(
        rowHeights: [CGFloat],
        spacing: CGFloat,
        controlHairline: CGFloat? = nil,
        budget: CGFloat
    ) -> CGFloat {
        guard !rowHeights.isEmpty else { return budget }
        let gaps = spacing * CGFloat(max(rowHeights.count - 1, 0))
        let separator = controlHairline.map { spacing + $0 } ?? 0
        let summed = rowHeights.reduce(0, +) + gaps + separator
        return min(max(summed, spacing), budget)
    }
}
