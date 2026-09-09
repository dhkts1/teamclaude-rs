import CoreGraphics

/// The wrapped height of a string, measured OUTSIDE any view graph.
///
/// A protocol with no implementation in this target, and that is the whole
/// point of it. The only honest way to know how tall `"$12.4 today · $3.10/hr ·
/// opus-5 62% · …"` renders is to ask a text engine, and the text engine on
/// this platform is `NSString.boundingRect(with:options:attributes:)` — which
/// needs AppKit, which lives in the `TcrBar` executable target. Depending on it
/// here would mean adding `"TcrBar"` to the test target to test this
/// arithmetic, and `Package.swift:39-43` gives that target `TcrBarCore` alone
/// on purpose: `PanelHeight.swift:20-23` records what a view-private height
/// helper cost the last time, which was a rule a wrapped header broke silently
/// with every test green.
///
/// So the measurement is injected. `TcrBar` conforms an AppKit implementation
/// to this and hands it in; the tests hand in a deterministic fake. Neither
/// side of that is a compromise — see ``PanelSize`` for why the estimate is
/// allowed to be wrong in the first place.
///
/// `width` is the width the string will actually be DRAWN at, not the panel's:
/// the caller subtracts the gutters (``PanelSize/Geometry/textWidth``).
/// Measuring at the panel's full width under-counts every wrapping line in the
/// panel, and under-counting is the one direction that puts the footer over the
/// edge.
public protocol TextMetrics {
    func height(of text: String, fontSize: CGFloat, width: CGFloat) -> CGFloat
}

/// The size the popover should be AUTHORED at, as arithmetic a test can run.
///
/// Nothing calls this yet. It is phase 2 of
/// `docs/plans/panel-sizing-generalization.md`; phase 3 computes it beside the
/// popover's real `contentSize` and logs the delta, and phase 4 assigns it.
///
/// **The one property everything here exists to hold: a wrong estimate lands
/// on the account list, never on the footer.** The panel today sizes itself to
/// its content (`sizingOptions = [.preferredContentSize]`), so a header that
/// wrapped further than anyone predicted pushed Quit and the three settings
/// checkboxes off the bottom of the popover, with no scroll region around them
/// to recover with — `PanelHeight.swift:6-19` records that bug and the whole of
/// `headerOverflow`/`listBudget` exists to charge the list for it in advance.
/// Charging in advance requires the prediction to be right. This does not: the
/// container is authored, every child but one hugs its content, and the
/// account list's `ScrollView` is the single flexible child that absorbs the
/// residual. An estimate that is 50pt short scrolls 50pt early. An estimate
/// that is 50pt short under the old shape moved the footer 50pt off screen.
///
/// That is also why nothing here tries to be exact. The measured gap between
/// five wrapping chrome lines drawn at once and a one-line-each prediction is
/// 49.45pt (`docs/plans/panel-sizing-generalization.md` §4c), and whether
/// TextKit's wrap point agrees with SwiftUI `Text`'s to the point is the one
/// assumption the design could not test without running the panel. It does not
/// have to: the residual scroll region is what makes being wrong survivable,
/// and it is not decoration.
///
/// The two CLAMPS stay in ``PanelHeight`` under their current names and are
/// read from there, never restated — `scripts/tcrbar-palette.py:501-502` reads
/// that file by path and publishes both as design tokens, so moving or
/// renaming them deletes two tokens and reddens the pre-commit gate without
/// saying why. Everything else this arithmetic runs on is a PARAMETER supplied
/// by the caller, for the reason `PanelHeight`'s own doc-comment gives: a test
/// that drives the inputs with its own values tests the rule, while a test that
/// restates a clamp proves an identity about a number.
public enum PanelSize {
    /// One wrapping line of chrome — a header or footer string, with the size
    /// of the font it is drawn in.
    ///
    /// `fontSize` rather than a font, because a font is an AppKit type and this
    /// target has no AppKit; the conforming ``TextMetrics`` on the other side
    /// owns the mapping from a size to a face. Every string the panel draws
    /// outside the account list is one of these, including the ones that
    /// usually render as a single line: `FleetView.swift` has nineteen
    /// `.fixedSize(horizontal: false, vertical: true)` sites and exactly one
    /// carries a `lineLimit`, so "this one never wraps" is not a fact about any
    /// of the other eighteen.
    public struct Line: Equatable, Sendable {
        public let text: String
        public let fontSize: CGFloat

        public init(_ text: String, fontSize: CGFloat) {
            self.text = text
            self.fontSize = fontSize
        }
    }

    /// The panel's geometry, as the caller declares it.
    ///
    /// Every figure here is an INPUT. `Tok` holds the shipped values
    /// (`Tokens.swift:250`, `:281-283`, `:312`) and lives in the executable
    /// target, which the tests cannot link — so `TcrBar` passes them in and the
    /// tests pass their own, which is what makes the arithmetic testable rather
    /// than the constants.
    public struct Geometry: Equatable, Sendable {
        /// `Tok.panelWidth`. Authored, never measured — the panel has been a
        /// fixed-width column since it was written.
        public let panelWidth: CGFloat

        /// `Tok.gutter`, the `.padding()` around `FleetView.body`. Costs the
        /// height twice and the text width twice.
        public let gutter: CGFloat

        /// `Tok.rowSpacing`, the spacing of `FleetView.body`'s own `VStack` —
        /// the gap between header, hairline, content, hairline and footer.
        public let sectionSpacing: CGFloat

        /// `Tok.tightSpacing`, the spacing INSIDE the header's and the footer's
        /// `VStack`s (`FleetView.swift:109`, `:450`).
        public let lineSpacing: CGFloat

        /// `Tok.rowSpacing` again, in its other role: the gap between two
        /// account cards inside the scrolling list.
        public let rowSpacing: CGFloat

        /// `Tok.hairlineWidth`. Two `Hairline()`s sit in the body, and their
        /// thickness is small enough to look ignorable — which is how the
        /// viewport came out 8.5pt shorter than its content once already
        /// (`PanelHeight.swift:102-113`).
        public let hairlineWidth: CGFloat

        /// A representative account row. Rows are NOT uniform height — that is
        /// exactly why the row-count cap was rejected twice in writing
        /// (`Tokens.swift:254`, `PanelHeight.swift:47`) — so this is an
        /// estimate and is treated as one. It is allowed to be wrong because
        /// the list is the residual; see the type's doc-comment.
        public let rowHeight: CGFloat

        /// Everything in the footer that does not wrap: the two button rows and
        /// the three settings checkboxes. Reserved whether or not the footer
        /// draws any text, because those controls are what the whole bug was
        /// about.
        public let fixedControlsHeight: CGFloat

        public init(
            panelWidth: CGFloat,
            gutter: CGFloat,
            sectionSpacing: CGFloat,
            lineSpacing: CGFloat,
            rowSpacing: CGFloat,
            hairlineWidth: CGFloat,
            rowHeight: CGFloat,
            fixedControlsHeight: CGFloat
        ) {
            self.panelWidth = panelWidth
            self.gutter = gutter
            self.sectionSpacing = sectionSpacing
            self.rowSpacing = rowSpacing
            self.lineSpacing = lineSpacing
            self.hairlineWidth = hairlineWidth
            self.rowHeight = rowHeight
            self.fixedControlsHeight = fixedControlsHeight
        }

        /// The width a chrome string is really drawn at: the panel less both
        /// gutters. The single most consequential line in this file to get
        /// wrong — measuring at `panelWidth` reports fewer wrapped lines than
        /// the panel will draw, and an under-estimate is the direction that
        /// costs the footer its place.
        public var textWidth: CGFloat { panelWidth - 2 * gutter }
    }

    /// The authored size, with the parts it is made of kept separately.
    ///
    /// ``contentSize`` is what phase 4 assigns. The four parts are public
    /// because phase 3's whole job is logging this against the popover's real
    /// size on a live fleet, and a bare total says only "wrong by 40pt" — the
    /// parts say which of the header, the footer or the list it was.
    public struct Plan: Equatable, Sendable {
        public let width: CGFloat
        public let headerHeight: CGFloat
        public let footerHeight: CGFloat
        public let listHeight: CGFloat

        /// The structure of `FleetView.body` itself: both gutters, both
        /// hairlines, and the gaps between its children.
        public let frameHeight: CGFloat

        public var chromeHeight: CGFloat { headerHeight + footerHeight + frameHeight }
        public var height: CGFloat { chromeHeight + listHeight }
        public var contentSize: CGSize { CGSize(width: width, height: height) }
    }

    /// How many children `FleetView.body`'s `VStack` has — header, `Hairline`,
    /// content, `Hairline`, footer (`FleetView.swift:93-103`) — so the gaps
    /// between them can be counted rather than guessed. A claim about that
    /// view, asserted in `PanelSizeTests`: add a sixth child without changing
    /// this and the authored size is one gap short, which is the last thing in
    /// the panel hanging over the edge again.
    public static let bodyChildren = 5

    /// The height to reserve for the scrolling account list.
    ///
    /// **Biased down by one row and one gap, on purpose.** The estimate will be
    /// wrong in one direction or the other; too short scrolls slightly early,
    /// too tall leaves a void under the last row that nothing fills and no
    /// scroll recovers. Scrolling early is the cheaper mistake. The bias is a
    /// whole row rather than a fudge factor so a reader can name the unit, and
    /// it is an order of magnitude larger than the terms it therefore does not
    /// need to model — the control-row separator's `spacing + hairlineWidth`
    /// (8.5pt, `PanelHeight.swift:102-113`) among them.
    ///
    /// The floor is ONE ROW, not ``PanelHeight/panelMinListHeight``. Those are
    /// different figures: 120 is the floor on the old list BUDGET — the space a
    /// long header was not allowed to take away — while this is the space the
    /// content actually wants. A fleet with one account should draw one row,
    /// not 120pt of panel with 78pt of nothing in it.
    ///
    /// The cap is ``PanelHeight/panelMaxHeight``, and it is a cap on the LIST
    /// and not on the panel — its own doc-comment says so
    /// (`PanelHeight.swift:44-50`). Capping the total instead is precisely how
    /// a long header came out of the footer.
    public static func listHeight(rowCount: Int, geometry: Geometry) -> CGFloat {
        guard rowCount > 0 else { return 0 }
        let natural =
            CGFloat(rowCount) * geometry.rowHeight
            + CGFloat(rowCount - 1) * geometry.rowSpacing
        let biased = natural - geometry.rowHeight - geometry.rowSpacing
        return min(PanelHeight.panelMaxHeight, max(geometry.rowHeight, biased))
    }

    /// The height of one stack of chrome lines: every line measured at the
    /// drawn width, plus the gaps BETWEEN them.
    ///
    /// `n - 1` gaps, and the empty stack reserves nothing at all — a footer
    /// with no text still gets its controls, but it does not also get a gap
    /// above nothing. Both are `VStack` semantics, and both are the kind of
    /// off-by-one that shows up as a panel a few points wrong on one state and
    /// right on every other.
    public static func stackHeight(
        _ lines: [Line], geometry: Geometry, metrics: TextMetrics
    ) -> CGFloat {
        guard !lines.isEmpty else { return 0 }
        let text = lines.reduce(CGFloat(0)) {
            $0 + metrics.height(of: $1.text, fontSize: $1.fontSize, width: geometry.textWidth)
        }
        return text + CGFloat(lines.count - 1) * geometry.lineSpacing
    }

    /// The authored size for a fleet of `rowCount` accounts, given the chrome
    /// strings actually on screen.
    ///
    /// `header` and `footer` are what the panel is DRAWING right now, not every
    /// line it could draw: most of them are conditional — the poll summary only
    /// on an unhealthy read (`FleetView.swift:127`), the update line only for
    /// two of four `UpdateState` cases, the spend line only when a row carries
    /// usage. Reserving a line that is not rendered is the same failure
    /// `PanelHeight.headerOverflow`'s `lineIsDrawn` parameter exists to
    /// prevent, arriving from the other end.
    public static func plan(
        rowCount: Int,
        header: [Line],
        footer: [Line],
        geometry: Geometry,
        metrics: TextMetrics
    ) -> Plan {
        // Header first, footer second — the order they are drawn in, and the
        // order a `TextMetrics` that records its asks will see. Nothing else is
        // measured: an implementation that asks the text engine about a string
        // the panel is not drawing is reserving height for a line that is not
        // there.
        let headerText = stackHeight(header, geometry: geometry, metrics: metrics)
        let footerText = stackHeight(footer, geometry: geometry, metrics: metrics)
        let gapAboveControls =
            (footer.isEmpty || geometry.fixedControlsHeight == 0) ? 0 : geometry.lineSpacing

        return Plan(
            width: geometry.panelWidth,
            headerHeight: headerText,
            footerHeight: footerText + gapAboveControls + geometry.fixedControlsHeight,
            listHeight: listHeight(rowCount: rowCount, geometry: geometry),
            frameHeight: 2 * geometry.gutter + 2 * geometry.hairlineWidth
                + CGFloat(bodyChildren - 1) * geometry.sectionSpacing
        )
    }

    /// ``plan(rowCount:header:footer:geometry:metrics:)`` for a decoded fleet.
    ///
    /// Counts `accounts`, which is every row the list draws — an unreadable row
    /// is reported in the header's own words (`Fleet.unreadableNotice`) and
    /// draws no card, so it is chrome here and not a row.
    public static func forFleet(
        _ fleet: Fleet,
        header: [Line],
        footer: [Line],
        geometry: Geometry,
        metrics: TextMetrics
    ) -> CGSize {
        plan(
            rowCount: fleet.accounts.count,
            header: header,
            footer: footer,
            geometry: geometry,
            metrics: metrics
        ).contentSize
    }
}
