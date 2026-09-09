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

    /// What the header's spend line takes BEYOND one rendered line — the figure
    /// ``listBudget(cap:headerOverflow:minimum:)`` subtracts.
    ///
    /// `lineIsDrawn` is the parameter this exists for. Both measurements arrive
    /// from SwiftUI preferences, and a preference is only emitted while the view
    /// that emits it is on screen: when the spend line stops rendering — an
    /// older proxy, or a read that went offline, both routine here — the last
    /// measured pair is simply the last thing anyone said. Subtracting it goes
    /// on charging the account list 14 to 28pt for a header that is no longer
    /// there, for the rest of the session: dead space under the last row and a
    /// scrollbar on a fleet that would have fit.
    ///
    /// The observers moved outside the branch that renders the line so the
    /// state resets on its own, and this makes that reset unnecessary as well
    /// as true: a header with no line has no overflow, whatever the last
    /// measurement happened to be. A rule with two independent reasons to hold
    /// is the one that survives a refactor of either.
    public static func headerOverflow(
        lineHeight: CGFloat,
        oneLineHeight: CGFloat,
        lineIsDrawn: Bool
    ) -> CGFloat {
        guard lineIsDrawn else { return 0 }
        return lineHeight - oneLineHeight
    }

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

    /// The grain every `GeometryReader` measurement this panel feeds into a
    /// `PreferenceKey` is snapped to before it is published.
    ///
    /// Matches `Tok.hairlineWidth` — the finest unit this panel already
    /// draws at — rather than a whole point, so quantizing costs no
    /// perceptible precision.
    public static let measurementGrain: CGFloat = 0.5

    /// Snap a raw `GeometryReader` measurement to ``measurementGrain``.
    ///
    /// `FleetView` publishes three of these every frame — one per account
    /// row plus the two spend-line measurements — through
    /// `.preference(key:value:)`, and `onPreferenceChange` re-fires (and
    /// re-triggers `@State`, and therefore another layout pass) whenever the
    /// published value is not bit-for-bit equal to the last one. A resize
    /// this arithmetic itself drives — `.frame(height: visibleRowsHeight(...))`
    /// sizes the very `ScrollView` whose rows are being measured — has no
    /// guarantee of reporting the identical `CGFloat` on consecutive passes:
    /// window resize and AppKit's safe-area-inset recomputation both round
    /// sub-pixel geometry a hair differently frame to frame, so the raw value
    /// can drift by float epsilon forever without ever landing on the same
    /// bits twice. That is a genuine non-terminating case for
    /// `onPreferenceChange`, which is what a runaway `NSPopover` layout
    /// (`_NSPopoverWindow`, "already had more Update Constraints in Window
    /// passes than there are views") looks like from AppKit's side — its
    /// guard is a *pass count*, not a convergence detector, so it fires
    /// exactly when this loop keeps going.
    ///
    /// Quantizing turns that infinite, arbitrarily-fine domain into a finite
    /// one: two measurements within half the grain of each other now publish
    /// the identical value, so once the *real* layout has settled (the rows'
    /// actual heights stop changing, even if AppKit keeps reporting them
    /// with fresh sub-pixel noise), the published preference stops changing
    /// too, `onPreferenceChange` stops firing, and the layout pass that
    /// triggered it is the last one.
    /// Internal, not public, and that is load-bearing. After ``settled(_:_:)``
    /// took over the publish path this had no caller outside this file, and the
    /// one edit that would silently undo the fix — restoring
    /// `PanelHeight.quantized(proxy.size.height)` at a `GeometryReader` emitter
    /// in `FleetView` — is the kind of tidy-up that reads as harmless. `FleetView`
    /// is in the `TcrBar` target and cannot reach an internal symbol in
    /// `TcrBarCore`, so that edit is now a compile error rather than a green test
    /// run with the fix dead. No test could have caught it: `Package.swift:39-43`
    /// gives the test target `TcrBarCore` only, on purpose.
    static func quantized(_ measurement: CGFloat) -> CGFloat {
        (measurement / measurementGrain).rounded() * measurementGrain
    }

    /// The value to publish for a freshly measured height, given what was
    /// published last.
    ///
    /// ``quantized(_:)`` alone is not enough, and under one centring it is
    /// actively harmful. Snapping to a grid is a uniform quantizer, and a
    /// quantizer inside a feedback loop is a source of limit cycles rather
    /// than a damper on them: a raw measurement sitting on a cell BOUNDARY
    /// snaps alternately to the two neighbouring grid points, so a signal that
    /// was merely noisy becomes a clean two-state oscillation that never
    /// settles. Measured on a model of this arithmetic, not on a running panel,
    /// with the shipped fixture as its control: the same 1e-5 jitter publishes
    /// one value when centred on 118.0 — which is the centring
    /// `PanelHeightTests` has always used, and why it never caught this — and
    /// two when centred on 118.25, republishing on 199 of 200 passes. Every republish re-fires the
    /// observer, moves the list's frame by half a point and runs
    /// `NSHostingView.setFrameSize`.
    ///
    /// What that costs is a pass budget, and that is the whole claim. Whether
    /// this is the loop that aborts on the crashing machine is NOT established
    /// and nothing here should be read as saying it is: the same investigation
    /// records that the crash cycle reads no preference at all
    /// (`MenuBarShell.swift`, the #208 comment), and that all three 0.2.43 stacks
    /// contain no TcrBar frame anywhere. This removes a demonstrated 199-in-200
    /// republish. It may or may not remove the crash.
    ///
    /// A dead band fixes what the grid cannot: hold the previous value while
    /// the new one is within one grain of it, and only then snap. The band has
    /// to see the RAW measurement — applying it after ``quantized(_:)`` cannot
    /// work, because two adjacent grid points differ by exactly
    /// ``measurementGrain`` and `0.5 < 0.5` is false, so the alternation would
    /// pass straight through. That is why the three `GeometryReader` emitters
    /// publish `proxy.size.height` unrounded now and this function is the only
    /// place the grid is applied.
    ///
    /// A genuine content change still moves: it is larger than a grain, so it
    /// falls outside the band on the first pass. Continuous drift is not
    /// swallowed either — the band compares against the last PUBLISHED value, so
    /// the error never exceeds one grain and the value keeps moving.
    ///
    /// Two costs, both found by review rather than by me, both recorded here
    /// rather than argued away:
    ///
    /// A permanent SUB-GRAIN step is held indefinitely — a row that genuinely
    /// settles at 40.4 goes on publishing 40.0, where quantizing alone would
    /// have published 40.5. The error is one-sided, and
    /// ``visibleRowsHeight(rowHeights:spacing:controlHairline:budget:)`` sums it
    /// across rows, so a fleet under budget can get a viewport up to
    /// `0.5 × rowCount` shorter than its content — the same symptom class the
    /// `controlHairline` term above exists to fix. Bounded, and it only bites
    /// below the cap; above it the list clamps and scrolls regardless.
    ///
    /// And the emitters publishing raw gives up a gate that used to sit below
    /// this code: `onPreferenceChange` requires `Equatable` and skips the
    /// callback when the published value is unchanged, so on a quantized
    /// preference SwiftUI itself absorbed sub-grain jitter and the closure never
    /// ran. It now runs every pass and writes `@State` with a value equal to
    /// what is already there. That is inert as far as `.frame(height:)` is
    /// concerned — the same `CGFloat` produces the same frame — but SwiftUI does
    /// not document it, and it was not measurable without a running panel.
    public static func settled(_ previous: CGFloat, _ measured: CGFloat) -> CGFloat {
        abs(measured - previous) < measurementGrain ? previous : quantized(measured)
    }

    /// The per-row form. A row absent from `measured` has gone from the fleet
    /// and is dropped rather than held; a row with no previous value is
    /// published on the grid, since there is nothing to hold to.
    public static func settled(
        _ previous: [String: CGFloat],
        _ measured: [String: CGFloat]
    ) -> [String: CGFloat] {
        var out = measured
        for (id, value) in measured {
            out[id] = previous[id].map { settled($0, value) } ?? quantized(value)
        }
        return out
    }
}
