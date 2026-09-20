import CoreGraphics

/// The Peers tab's height budget, as arithmetic a test can run.
///
/// Same reason ``PanelHeight`` and ``PanelSize`` live here rather than in the
/// view: `Package.swift:39-43` gives the test target `TcrBarCore` alone, so a
/// height helper private to `PeersTabV4` could not be tested at all, and
/// `PanelHeight.swift:20-23` records what that cost the last time a wrapped
/// header broke a rule with every test green.
///
/// # Why this is not a fifth region in `PanelSize.Plan`
///
/// An earlier design asked for `peerSectionHeight` "as a fifth region in
/// `PanelSize.Plan`", and that shape was right for the design it was written
/// against: a peer SECTION stacked on the Accounts tab, so its height adds to
/// the account list's. That was replaced with a fourth TAB ("maybe another
/// tab called peers?"), and a tab is mutually
/// exclusive with the account list, only one of them is ever on screen. A
/// fifth addend would therefore reserve height for a list that is not drawn,
/// which is the same failure `PanelSize.plan`'s own doc-comment names for a
/// chrome line that is not rendered, arriving from the other end.
///
/// So the peer section occupies the CONTENT region: it is what
/// ``PanelSize/Plan/listHeight`` holds while the Peers tab is selected, and
/// the identity to hold is the one the gate actually cares about:
/// `height == headerHeight + footerHeight + frameHeight + listHeight`, with
/// the footer keeping its full height at every peer count.
///
/// # The property this exists to hold
///
/// **Growth past the cap comes out of the peer list, never out of the
/// footer.** ``PanelHeight/panelMaxHeight`` is a cap on the LIST and not on
/// the panel (`PanelHeight.swift:44-50` says so in as many words), and
/// capping the total instead is exactly how a long header pushed Quit and the
/// settings checkboxes off the bottom of the popover once already. A Mac with
/// forty trusted peers must scroll its peer rows, not lose its footer.
///
/// # Why every figure is a parameter
///
/// `Tok` and `V4` live in the executable target, which the tests cannot link.
/// A test that restated a clamp would prove an identity about a number; a test
/// that drives the arithmetic with its own values tests the rule. The one
/// exception is the cap, which is read from ``PanelHeight/panelMaxHeight``
/// rather than passed, for the reason that file gives: an assertion about that
/// number IS an assertion about the running panel, and
/// `scripts/tcrbar-palette.py` publishes it as a design token.
public enum PeerPanelHeight {
    /// One peer row's SHAPE, how many lines it draws and whether it carries a
    /// meter. Not its height: that is ``Metrics``, which the caller owns.
    ///
    /// The mockup's rule 7 (the Peers tab mockup (kept outside the tree)): "a
    /// row is one line plus one sub line, so eight Macs still clear the 520 pt
    /// list cap". `subLines` is 1 for the ordinary row and 2 for the two rows
    /// that carry a wrapping sentence, the untrusted row explaining what
    /// Trust buys, and a lease meter's scale sentence, because that sentence
    /// is the one string on the tab with no bounded length.
    public struct Row: Equatable, Sendable {
        /// Sub lines under the name line. Never negative; a row with none is
        /// the name line alone.
        public let subLines: Int
        /// Whether the row draws a meter (a lease fraction or a gateway byte
        /// rate). Both meters are the same height (one bar and its label),
        /// so the SHAPE does not need to know which, and
        /// `PeersTabV4.PeerMeter` is what keeps the two from being confused.
        public let hasMeter: Bool

        public init(subLines: Int, hasMeter: Bool) {
            self.subLines = max(0, subLines)
            self.hasMeter = hasMeter
        }

        /// The ordinary trusted row: a name line, one sub line, no meter.
        public static let plain = Row(subLines: 1, hasMeter: false)
        /// A row carrying a lease or gateway meter and its scale sentence.
        public static let metered = Row(subLines: 2, hasMeter: true)
    }

    /// What the drawn tab charges per part. Every value is an input; `V4`
    /// holds the shipped ones and passes them in.
    public struct Metrics: Equatable, Sendable {
        /// The name line's box (`V4.lineHeight(V4.nameSize)`).
        public let nameLineHeight: CGFloat
        /// One sub line's box (`V4.lineHeight(V4.muteSize)`).
        public let subLineHeight: CGFloat
        /// A meter: its bar, its own top margin and its value line.
        public let meterHeight: CGFloat
        /// Both card edges, padding twice plus both borders
        /// (`2 * V4.cardInsetV`).
        public let cardChrome: CGFloat
        /// The gap between two cards (`V4.cardGap`).
        public let cardGap: CGFloat
        /// Everything on the tab that is NOT a peer row: the Find card, the
        /// Share card, the section head and the count line. Reserved at every
        /// peer count, including zero, because those two switches are the tab.
        public let fixedChrome: CGFloat
        /// One knock card, drawn whole: its name line, its address line, the
        /// three lines its sentence wraps to, its two buttons, and the card's
        /// own edges.
        ///
        /// Charged PER PENDING KNOCK rather than folded into
        /// ``fixedChrome``, which is reserved at every count: a knock is the
        /// one card on this tab that is usually absent and gone in ten
        /// minutes when it is not.
        public let knockCardHeight: CGFloat

        public init(
            nameLineHeight: CGFloat,
            subLineHeight: CGFloat,
            meterHeight: CGFloat,
            cardChrome: CGFloat,
            cardGap: CGFloat,
            fixedChrome: CGFloat,
            knockCardHeight: CGFloat
        ) {
            self.nameLineHeight = nameLineHeight
            self.subLineHeight = subLineHeight
            self.meterHeight = meterHeight
            self.cardChrome = cardChrome
            self.cardGap = cardGap
            self.fixedChrome = fixedChrome
            self.knockCardHeight = knockCardHeight
        }
    }

    /// One row's drawn height: its lines, its meter, and its card's own edges.
    public static func rowHeight(_ row: Row, metrics: Metrics) -> CGFloat {
        metrics.nameLineHeight
            + CGFloat(row.subLines) * metrics.subLineHeight
            + (row.hasMeter ? metrics.meterHeight : 0)
            + metrics.cardChrome
    }

    /// What the peer LIST wants: every row and the gap above each one.
    /// Uncapped, on purpose, the cap is a separate decision and a caller that
    /// needs to know whether the list overflows needs this number, not the
    /// clamped one.
    ///
    /// The two switch cards are NOT in here, and that is the load-bearing
    /// part. ``PanelHeight/panelMaxHeight`` is documented as the cap on the
    /// LIST: the list clears the 520 pt cap at seven peers. The switches are
    /// pinned chrome, a Find
    /// switch that scrolled out of reach under a long peer list is the same
    /// class of bug as a footer that did.
    public static func listHeight(rows: [Row], metrics: Metrics) -> CGFloat {
        let content = rows.reduce(CGFloat(0)) { $0 + rowHeight($1, metrics: metrics) }
        // A gap ABOVE each row, because each sits under either the Find card
        // or the row before it, the same `n` gaps `AccountsTabV4` charges its
        // cards, not `n - 1`.
        return content + CGFloat(rows.count) * metrics.cardGap
    }

    /// The height to give the SCROLLING peer list: what the rows want, capped
    /// at ``PanelHeight/panelMaxHeight``.
    ///
    /// This is the number the running view frames its scroll view to, and the
    /// clamp lives here rather than at the call site so the drawn viewport and
    /// ``peerSectionHeight(rows:metrics:)`` cannot disagree about the cap. A
    /// `min` restated in `PeersTabV4` would be the same decision in two
    /// places, and the one that drifts is the one no test runs.
    public static func listViewportHeight(rows: [Row], metrics: Metrics) -> CGFloat {
        min(PanelHeight.panelMaxHeight, listHeight(rows: rows, metrics: metrics))
    }

    /// What the pending knock cards cost: one card each, and the gap above
    /// each one, the same `n` gaps and not `n - 1` that
    /// ``listHeight(rows:metrics:)`` charges its rows, because each knock card
    /// sits under either the Find card or the knock before it.
    ///
    /// OUTSIDE the capped list on purpose. The knock cards are drawn above
    /// "Other Macs" and outside the scroll view (`PeersTabV4`), so charging
    /// them to the list would reserve height inside a region that is clamped
    /// and let the growth come out of the rows instead.
    public static func knockHeight(pendingKnocks: Int, metrics: Metrics) -> CGFloat {
        CGFloat(max(0, pendingKnocks)) * (metrics.knockCardHeight + metrics.cardGap)
    }

    /// The height to give the whole Peers tab: its two switches, every card a
    /// Mac asking to connect draws, plus a peer list clamped to
    /// ``PanelHeight/panelMaxHeight``.
    ///
    /// The clamp is the whole point of this function. Past the cap the rows
    /// scroll; the switches and the footer, which are drawn outside that
    /// region, keep every point they had at one peer. Clamping the panel's
    /// TOTAL instead (or not clamping at all) is the recorded bug
    /// `PanelHeight`'s header describes and `ffe8a86` fixed.
    ///
    /// # Why the knocks are counted here at all
    ///
    /// They were charged NOWHERE. `fixedChrome` is the two switch cards, the
    /// section head and the count line, and the list is the trusted rows, so a
    /// panel holding a request to connect was sized as a panel holding none.
    /// That is vertical pressure the tab cannot see, and what gives way under
    /// it is a sentence: the knock card's own help line rendered cut mid-word
    /// in the running panel. Making ``MuteText`` wrap without paying for the
    /// lines it wraps to would only move the squeeze onto some other line.
    public static func peerSectionHeight(
        rows: [Row], pendingKnocks: Int = 0, metrics: Metrics
    ) -> CGFloat {
        metrics.fixedChrome + knockHeight(pendingKnocks: pendingKnocks, metrics: metrics)
            + listViewportHeight(rows: rows, metrics: metrics)
    }

    /// Whether the peer list overflows its cap at this peer count, which is
    /// the question "do seven peers still clear the 520 pt cap" is asking.
    public static func overflows(rows: [Row], metrics: Metrics) -> Bool {
        listHeight(rows: rows, metrics: metrics) > PanelHeight.panelMaxHeight
    }

    /// The authored panel size with the Peers tab selected.
    ///
    /// ``PanelSize/plan(rowCount:header:footer:geometry:metrics:)``'s own
    /// header and footer arithmetic, unchanged and not restated, this only
    /// replaces the content region with ``peerSectionHeight(rows:metrics:)``.
    /// Composed rather than copied so a change to the panel's frame or footer
    /// reaches this path too.
    public static func plan(
        rows: [Row],
        pendingKnocks: Int = 0,
        header: [PanelSize.Line],
        footer: [PanelSize.Line],
        geometry: PanelSize.Geometry,
        metrics: Metrics,
        textMetrics: TextMetrics
    ) -> PanelSize.Plan {
        let chrome = PanelSize.plan(
            rowCount: 0, header: header, footer: footer,
            geometry: geometry, metrics: textMetrics)
        return PanelSize.Plan(
            width: chrome.width,
            headerHeight: chrome.headerHeight,
            footerHeight: chrome.footerHeight,
            listHeight: peerSectionHeight(
                rows: rows, pendingKnocks: pendingKnocks, metrics: metrics),
            frameHeight: chrome.frameHeight
        )
    }
}
