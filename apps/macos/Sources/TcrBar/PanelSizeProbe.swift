import AppKit
import TcrBarCore

/// The AppKit half of the panel-sizing instrument: the text engine
/// ``TextMetrics`` is a protocol for, and the description of `FleetView`'s
/// chrome that `PanelSize` needs in order to predict a height.
///
/// It lives in this target because `TcrBarCore` deliberately cannot import
/// AppKit — `Package.swift`'s doc-comment and `PanelHeight.swift:20-23` are one
/// decision, and `docs/plans/panel-sizing-generalization.md` §10 item 5 forbids
/// the obvious shortcut of adding `"TcrBar"` to the test target. So the
/// arithmetic sits in the library where a test runs it, and the two things a
/// test cannot have — a font, and a view — sit here.
///
/// Nothing here changes what the panel draws. Phase 3 computes a prediction
/// beside the popover's real `contentSize` and logs the pair; `sizingOptions`
/// is untouched and `FleetView` is not opened at all.

/// ``TextMetrics`` through TextKit.
///
/// `NSString.boundingRect(with:options:attributes:)` is what
/// `docs/plans/panel-sizing-generalization.md` §2 specifies, and its value is
/// that it runs OUTSIDE any view graph: the wrapped height of a string is
/// geometry, and the design's whole distinction is between measuring it in a
/// text engine before the layout pass and measuring it with a `GeometryReader`
/// inside the pass that consumes the answer.
///
/// **Two known sources of under-measurement, both deliberate and both the
/// reason phase 3 exists rather than phase 4 shipping directly.** The protocol
/// carries a font SIZE and not a font, so the semibold faces `FleetView` uses
/// for `.headline` and `.subheadline` are measured at the regular weight and
/// come out narrower than they draw; and whether TextKit's wrap point agrees
/// with SwiftUI `Text`'s at all is the one assumption the design review could
/// not test without running the panel. Neither is fixed here. They are what the
/// log line is for.
struct AppKitTextMetrics: TextMetrics {
    func height(of text: String, fontSize: CGFloat, width: CGFloat) -> CGFloat {
        guard width > 0 else { return 0 }
        // An empty string measures zero, and a `Text("")` in a `VStack` draws a
        // full line box. Measuring a space instead keeps the two agreeing —
        // reserving nothing for a line that is on screen is the direction that
        // costs the footer its place.
        let subject = text.isEmpty ? " " : text
        let box = NSSize(width: width, height: .greatestFiniteMagnitude)
        let rect = (subject as NSString).boundingRect(
            with: box,
            // `.usesLineFragmentOrigin` is what makes this a MULTI-line
            // measurement; without it the call answers for one line and every
            // wrapping string in the panel is reported at a third of its
            // height. `.usesFontLeading` matches how TextKit lays the text out
            // when it is actually drawn.
            options: [.usesLineFragmentOrigin, .usesFontLeading],
            attributes: [.font: NSFont.systemFont(ofSize: fontSize)])
        // Up, never down. A half-point lost per line is a line lost over a tall
        // header, and the estimate is only allowed to be wrong in the direction
        // the residual scroll region absorbs.
        return rect.height.rounded(.up)
    }
}

/// What the shell hands `PanelSize` about `FleetView`, and the prediction it
/// gets back.
///
/// Every figure here is a claim about a view in this target, which is why it is
/// beside that view rather than in the library: `Tok`'s tokens are exact and
/// read straight from `Tokens.swift`, while the two heights nobody declares —
/// a card in the account list, and the footer's block of controls — are
/// ESTIMATES, marked as such, and are precisely what the phase-3 log line
/// exists to correct.
enum PanelSizeProbe {
    static let metrics: TextMetrics = AppKitTextMetrics()

    /// A representative account card.
    ///
    /// Rows are not uniform height — that is why a row-count cap was rejected
    /// twice in writing (`Tokens.swift:254`, `PanelHeight.swift:47`) — so this
    /// is an average and is treated as one. 42pt is the repo's own figure for
    /// one row (`docs/plans/panel-sizing-generalization.md` §8: "a fleet that
    /// really has one row should draw 42pt"). Being wrong here moves the list,
    /// which is the part the design lets be wrong.
    static let estimatedRowHeight: CGFloat = 42

    /// The footer's controls, which reserve their height whether or not the
    /// footer draws any text above them. They are what the original bug pushed
    /// off the bottom of the popover (`PanelHeight.swift:6-19`), so they are
    /// reserved unconditionally.
    ///
    /// Summed from `FleetView.footer` (`:449-494`) at the rows it draws, with
    /// the six `Tok.tightSpacing` gaps between the seven of them:
    ///
    /// | row | pt | from |
    /// |---|---|---|
    /// | `fleetActions` | 24 | one bordered button row |
    /// | `appActions` | 28 | the same, plus its `.padding(.top, tightSpacing)` (`:479`) |
    /// | `launchAtLogin` | 29 | toggle 16 + its detail caption 13 (`:854`) |
    /// | `startServerToggle` | 29 | toggle 16 + the supervision warning (`:743`) |
    /// | `keepAwakeToggle` | 29 | toggle 16 + the AC-power caption (`:796`) |
    /// | `dangerZone` | 32.5 | top padding 4 + `Hairline` 0.5 + gap 4 + button row 24 |
    /// | six gaps | 24 | `VStack(spacing: Tok.tightSpacing)` (`:450`) |
    ///
    /// An estimate, and the least certain number in this file: SwiftUI's
    /// control heights are not declared anywhere, and the three captions are
    /// conditional. It is here rather than hidden inside the arithmetic so the
    /// reader of a log line can see which number was assumed.
    static let estimatedFixedControlsHeight: CGFloat = 195.5

    /// `Tok`'s figures, passed in rather than read from the library — `Tok`
    /// lives in this target and `TcrBarCore` cannot see it, which is what keeps
    /// the arithmetic testable without linking a view.
    static let geometry = PanelSize.Geometry(
        panelWidth: Tok.panelWidth,
        gutter: Tok.gutter,
        sectionSpacing: Tok.rowSpacing,
        lineSpacing: Tok.tightSpacing,
        rowSpacing: Tok.rowSpacing,
        hairlineWidth: Tok.hairlineWidth,
        rowHeight: estimatedRowHeight,
        fixedControlsHeight: estimatedFixedControlsHeight
    )

    /// The chrome strings the HEADER is drawing right now — the same
    /// conditions `FleetView.header` (`:106-155`) draws them under, in the same
    /// order.
    ///
    /// Reserving a line the panel is not rendering is the failure
    /// `PanelHeight.headerOverflow`'s `lineIsDrawn` parameter exists to
    /// prevent, arriving from the other end, so every `if` in the view is
    /// mirrored here rather than approximated by a worst case.
    static func headerLines(state: PollState, update: UpdateState) -> [PanelSize.Line] {
        var lines: [PanelSize.Line] = [
            // The title row. One line by construction — the timestamp beside it
            // is in the same `HStack` — but it still costs its height.
            PanelSize.Line("tcr fleet", fontSize: NSFont.preferredFont(forTextStyle: .headline).pointSize)
        ]
        if !state.isHealthyRead {
            lines.append(PanelSize.Line(state.summary, fontSize: Tok.secondaryFontSize))
        }
        if case .loaded(let fleet) = state, !fleet.accounts.isEmpty {
            // Concatenated `Text` runs, drawn as one wrapping paragraph
            // (`FleetView.swift:236-249`) — so it is measured as one string,
            // not as one line per tally.
            let tallies = fleet.breakdown.map { " · \($0.label)" }.joined()
            lines.append(
                PanelSize.Line(
                    fleet.capacitySummary + tallies,
                    fontSize: NSFont.preferredFont(forTextStyle: .subheadline).pointSize))
        }
        if case .loaded(let fleet) = state, let usage = fleet.usageSummaryLine {
            lines.append(PanelSize.Line(usage, fontSize: Tok.secondaryFontSize))
        }
        if let message = update.headerMessage {
            lines.append(PanelSize.Line(message, fontSize: Tok.secondaryFontSize))
        }
        return lines
    }

    /// The chrome strings the FOOTER is drawing, above its fixed controls
    /// (`FleetView.footer:455-467`).
    ///
    /// `server <sha>` shares the server summary's `HStack` and carries
    /// `.lineLimit(1)`, so it adds no height of its own and is not a line here.
    /// `loginError` (`:481-486`) is `FleetView`'s own `@State` and the shell
    /// cannot see it; it is left out rather than guessed at, which makes the
    /// prediction short on exactly the ticks a login has just failed — a known
    /// gap, and one a reader of the log can identify because it does not move
    /// with anything else on the line.
    static func footerLines(server: ServerController.State) -> [PanelSize.Line] {
        [PanelSize.Line(server.summary, fontSize: Tok.secondaryFontSize)]
    }

    /// The prediction for this tick, beside what the popover actually is.
    static func delta(
        state: PollState,
        update: UpdateState,
        server: ServerController.State,
        actualContentSize: CGSize,
        isPanelShown: Bool
    ) -> PanelSizeDelta {
        let header = headerLines(state: state, update: update)
        let footer = footerLines(server: server)
        // Accounts, not rows-in-general: an undecodable row is reported in the
        // header's own words and draws no card, so it is chrome and not a row.
        let rowCount: Int = {
            if case .loaded(let fleet) = state { return fleet.accounts.count }
            return 0
        }()
        let plan = PanelSize.plan(
            rowCount: rowCount,
            header: header,
            footer: footer,
            geometry: geometry,
            metrics: metrics)
        return PanelSizeDelta(
            predicted: plan,
            actualContentSize: actualContentSize,
            rowCount: rowCount,
            headerLines: header.count,
            footerLines: footer.count,
            isPanelShown: isPanelShown)
    }
}
