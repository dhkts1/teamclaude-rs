import SwiftUI
import TcrBarCore

/// The dropdown panel.
///
/// Its one hard rule: never render a blank list. Every state that is not "a fleet
/// decoded cleanly" gets an explicit, distinguishable banner — `tcr` missing, the
/// poll failing (usually: no server), and an offline read whose counters are
/// structurally zero are three different facts, and an operator who cannot tell
/// them apart will misread the panel.
struct FleetView: View {
    @ObservedObject var poller: StatusPoller
    @ObservedObject var server: ServerController
    @ObservedObject var loginItem: LoginItem
    @ObservedObject var accounts: AccountController
    /// The identity-bound control account. See ``ControlAccountController``.
    @ObservedObject var control: ControlAccountController
    /// Owned by the app, for the same reason the poller is: the panel is a view
    /// and the mode is not, and an assertion released when the view went away
    /// would be a keep-awake control that keeps nothing awake. Under
    /// `MenuBarExtra` that teardown happened on every close; hosted in a popover
    /// it need not — which changes when that bug would bite, not whether it
    /// would.
    @ObservedObject var awake: AwakeController
    @ObservedObject var updater: Updater
    /// Mutating group membership from each row's right-click menu — the
    /// only way to change it from this panel now that the section-header
    /// menus are gone. See ``GroupController``.
    @ObservedObject var groupController: GroupController
    /// Deleting an account from each row's gear menu. See
    /// ``RemoveAccountController``.
    @ObservedObject var removeController: RemoveAccountController
    /// Owned by the app so it survives the panel closing; bound here so the
    /// checkbox and the launch path can never disagree about its value.
    @Binding var startServerAtLaunch: Bool

    /// Render the account list unscrolled, for the PNG harness only.
    ///
    /// `ImageRenderer` does not rasterise `ScrollView` content — measured, not
    /// assumed: the harness first produced a panel with a correct header above an
    /// empty void, and giving the scroll area a generous fixed height changed the
    /// output not by one byte. The scroll view is the thing that does not draw.
    ///
    /// So a snapshot lays the rows out in a plain stack. That is also the better
    /// review artifact: every row is visible at once instead of clipped to
    /// whatever the panel happens to show. Runtime is untouched — the default is
    /// `false` and the live panel still scrolls.
    var snapshotMode: Bool = false
    /// Opens the "What's new" window. A closure rather than the controller so
    /// the render harness passes `{}` and can neither fetch nor open anything.
    var onWhatsNew: () -> Void = {}
    /// Opens the Settings window (gear button, `⌘,`) — `data/plans/
    /// settings-window-bridge.md`. Same closure shape as ``onWhatsNew``, and
    /// for the same reason: the render harness passes `{}`.
    var onSettings: () -> Void = {}

    /// Surfaced in place rather than swallowed: a button that silently does
    /// nothing is worse than one that says why.
    @State private var loginError: String?

    /// Honour the system Reduce Motion setting, the way ``QuotaBar`` does.
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    /// Measured height of every child the list draws — each account row, each
    /// group heading and each band heading — keyed by that child's identity in
    /// the list, NOT by account id.
    ///
    /// **Keyed by ``FleetSectionRow/id``, and that is load-bearing.** An account
    /// in two groups is drawn twice (see ``accountList(_:)`` for the override
    /// that makes that correct), so `Account.id` — the account name — is no
    /// longer unique among the drawn rows. Under the old keying the two rows of
    /// one account wrote the same key, one measurement overwrote its twin, and
    /// `visibleRowsHeight(for:)` then summed n−1 heights for n rows: a viewport
    /// short of its own content by a whole row for every duplicated account,
    /// which reads as a list clipped mid-card. The composite id is unique per
    /// drawn row by construction (`FleetSectionsTests`), so each row owns its
    /// own slot.
    ///
    /// The headings share this dictionary because they share the same problem:
    /// they are children of the same `VStack` and cost the viewport their own
    /// height plus a gap, and nothing else measures them. Their keys carry the
    /// `b:`/`h:` prefixes from ``bandHeightKey(_:)`` and ``groupHeightKey(_:)``,
    /// which cannot collide with a row key — every row key starts with a
    /// ``FleetGroupKey/token``, i.e. `g:` or `u:`.
    ///
    /// A `ScrollView` has a flexible ideal height, and the window this panel
    /// lives in sizes itself to its content's *ideal* height — so a scroll view
    /// carrying only a `maxHeight` collapses to roughly one row no matter how
    /// many accounts the fleet has. Measuring the rows and giving the scroll
    /// view a concrete height is what makes the panel grow with the fleet.
    ///
    /// That was true of the `MenuBarExtra` window this panel used to live in and
    /// is equally true of the `NSPopover` it lives in now, whose hosting
    /// controller is set to `sizingOptions = [.preferredContentSize]`
    /// (`MenuBarShell.swift`) for exactly this reason.
    ///
    /// Per-child rather than one summed total because rows are not uniform
    /// height — the needs-relogin state and several conditional detail lines all
    /// grow a row. `visibleRowsHeight(for:)` sums all of these and clamps the
    /// total to what `PanelHeight.listBudget` leaves of `Tok.panelMaxHeight`.
    @State private var rowHeights: [String: CGFloat] = [:]

    /// The spend line as drawn, and the height one line of it would be.
    ///
    /// That line wraps rather than truncates and has no bounded length, so a
    /// busy fleet's summary is two or three lines — height the panel used to
    /// add on top of its own cap, pushing Quit and the checkboxes off the
    /// bottom. The difference between these two comes out of the LIST's budget
    /// instead (`PanelHeight.listBudget`), so the panel's total height is what
    /// it was before this line existed.
    ///
    /// Both are measured rather than computed: the font, the panel width and
    /// the string itself decide where it wraps, and the baseline is a hidden
    /// one-line `Text` in the same font drawn as an overlay, which takes part
    /// in no layout of its own.
    @State private var usageLineHeight: CGFloat = 0
    @State private var usageLineBaseline: CGFloat = 0

    /// Accounts / Sessions / Tools (F2 + F3, `panel-tabs-bridge.md`).
    /// `.accounts` selected by default, per the bridge.
    @State private var selectedTab: PanelTab = .accounts

    /// The Claude Code session files joined to the wire's `Session`s for the
    /// Sessions tab — see ``SessionFiles``. Read once on appear and again
    /// every poll; never on every render, since ``SessionFiles/read(directory:)``
    /// touches disk.
    @State private var sessionFiles: [String: SessionFile] = [:]

    /// Every stored property above still gets its usual default — this exists
    /// only so ``RenderStates`` can seed ``selectedTab`` deterministically.
    /// `@State`'s wrapped value can only be seeded through
    /// `State(initialValue:)` in an initializer; a plain assignment in
    /// `.onAppear` would work for the live app but leaves the render
    /// harness's very first (and only) frame racing SwiftUI's own appearance
    /// timing, which `ImageRenderer` does not document.
    init(
        poller: StatusPoller,
        server: ServerController,
        loginItem: LoginItem,
        accounts: AccountController,
        control: ControlAccountController,
        awake: AwakeController,
        updater: Updater,
        groupController: GroupController,
        removeController: RemoveAccountController,
        startServerAtLaunch: Binding<Bool>,
        snapshotMode: Bool = false,
        onWhatsNew: @escaping () -> Void = {},
        onSettings: @escaping () -> Void = {},
        initialTab: PanelTab = .accounts,
        // Coordinator-flagged gap (2026-09-12): `snapshotMode` never reads
        // real session files (comment above, `body`'s `.onAppear`), so every
        // render-states Sessions/Tools fixture used to join to NOTHING and
        // read `.unknown` → the summary line's "idle" bucket for every
        // session, regardless of what the fixture's own `Session.tools`
        // narrative says. `RenderStates` seeds this so its fixture sessions
        // actually carry the busy/waiting/idle status their own doc-comments
        // describe, instead of the pixelmatch gate comparing a made-up
        // "0 busy · 0 waiting · 3 idle" against the mockup's real counts.
        initialSessionFiles: [String: SessionFile] = [:]
    ) {
        self.poller = poller
        self.server = server
        self.loginItem = loginItem
        self.accounts = accounts
        self.control = control
        self.awake = awake
        self.updater = updater
        self.groupController = groupController
        self.removeController = removeController
        self._startServerAtLaunch = startServerAtLaunch
        self.snapshotMode = snapshotMode
        self.onWhatsNew = onWhatsNew
        self.onSettings = onSettings
        self._selectedTab = State(initialValue: initialTab)
        self._sessionFiles = State(initialValue: initialSessionFiles)
    }

    var body: some View {
        VStack(alignment: .leading, spacing: Tok.rowSpacing) {
            header
            Hairline()
            content
            Hairline()
            footer
        }
        .padding(Tok.gutter)
        .frame(width: Tok.panelWidth)
        .background(Tok.panel)
        // `snapshotMode` never touches disk here: the render harness's session
        // ids are fixture strings that join to nothing real, so the read
        // would only ever populate an unused dictionary — see
        // `RenderStates.swift`'s sessions-tab fixtures.
        .onAppear { if !snapshotMode { sessionFiles = SessionFiles.read() } }
        .onChange(of: poller.lastPollAt) { _ in
            if !snapshotMode { sessionFiles = SessionFiles.read() }
        }
    }

    // MARK: Header

    private var header: some View {
        VStack(alignment: .leading, spacing: Tok.tightSpacing) {
            HStack {
                // v4-spec: panel title 17pt/700 — `.headline` resolves to
                // 13pt/semibold on macOS, well under the mockup's size.
                // Literal per the tokens-parity fence; swap for a `Tok` name
                // once one exists.
                Text("tcr fleet").font(Tok.panelTitleFont)
                Spacer()
                if let at = poller.lastPollAt {
                    // `docs/design/panel-tabs-mockup.html` F14: the header's
                    // trailing slot answers "is this current", not "what time
                    // is it" — the system clock is already a couple of
                    // centimetres away in the same menu bar. Computed once per
                    // render, same as `trailingStatus`'s `now:` parameter
                    // elsewhere in this file — there is no live timer driving
                    // a re-render between polls, so this reads as of the last
                    // paint, not truly live-ticking.
                    //
                    // v4-spec: freshness is 12.5pt/400, `--mute`, not `--dim`
                    // — `Tok.inkFaint` is this build's mute-role token
                    // (`Tokens.swift`'s own naming: "Tertiary text and
                    // hints"), so this corrects the colour role along with
                    // the size.
                    Text(freshnessLabel(since: at, now: Date()))
                        .font(Tok.tabular(Tok.freshnessFont))
                        .foregroundStyle(Tok.inkFaint)
                }
                // Gear opens Settings — in the header, not the footer, and
                // present regardless of which tab is selected: it is window
                // chrome, not tab content, the same reasoning the mockup's
                // own note gives for repeating it on all three tabs
                // (`docs/design/panel-tabs-mockup.html`). `⌘,` is the
                // platform's own settings shortcut.
                Button(action: onSettings) {
                    Image(systemName: "gearshape")
                }
                .buttonStyle(.plain)
                .foregroundStyle(Tok.inkDim)
                .keyboardShortcut(",", modifiers: [.command])
                .help("Settings… ⌘,")
                .accessibilityLabel("Settings")
            }
            // Only when the read is NOT healthy. On a healthy read this said
            // "13 accounts — live", which the tallies below say with more
            // detail. Every other state — pending, tool missing, command
            // failed, undecodable, stale — is carried nowhere else in the
            // panel, so it still prints, and it is then the most important
            // sentence on screen.
            if !poller.state.isHealthyRead {
                Text(poller.state.summary)
                    .font(Tok.secondaryFont).lineSpacing(Tok.secondaryLineSpacing)
                    .fixedSize(horizontal: false, vertical: true)
            }
            // F1: each tab answers a different question in this slot, so the
            // summary line switches with `selectedTab` rather than always
            // showing the Accounts one — `docs/design/panel-tabs-mockup.html`
            // draws three different sentences here, one per panel.
            if case .loaded(let fleet) = poller.state, !fleet.accounts.isEmpty,
                selectedTab == .accounts
            {
                capacitySummary(fleet)
            }
            if selectedTab == .accounts {
                usageSummary
            }
            if case .loaded(let fleet) = poller.state, selectedTab == .sessions,
                fleet.sessionsSupported, !fleet.sessions.isEmpty
            {
                sessionsSummaryLine(fleet)
            }
            if case .loaded(let fleet) = poller.state, selectedTab == .tools,
                fleet.sessionsSupported
            {
                toolsSummaryLine(fleet)
            }
            updateStateLine
        }
        // OUTSIDE `usageSummary`'s own `if`, and outside the subtree that
        // emits these two preferences — the same placement `RowHeightsKey`'s
        // observer already has on the ScrollView, and for the same reason.
        // Attached inside that branch, the emitter and its observers vanished
        // together the moment the spend line stopped rendering, so neither
        // closure fired and `usageLineHeight`/`usageLineBaseline` kept their
        // last values on a header that was no longer on screen — 14 to 28pt
        // still being subtracted from the account list's budget for the rest
        // of the session. That is not an exotic state: it is what the panel
        // shows the instant the proxy is restarted onto a build predating
        // `usage`, or the read goes offline.
        //
        // Here they fire with each key's `defaultValue` (0) when the emitting
        // subtree disappears, which is the reset — a header that draws no
        // spend line has no overflow, and `listBudget` gives the list the
        // whole cap back.
        .onPreferenceChange(UsageLineHeightKey.self) {
            usageLineHeight = PanelHeight.settled(usageLineHeight, $0)
        }
        .onPreferenceChange(UsageLineBaselineKey.self) {
            usageLineBaseline = PanelHeight.settled(usageLineBaseline, $0)
        }
    }

    /// The fleet's spend, burn rate, model mix and cache hit rate, on one line:
    /// `$12.4 today · $3.10/hr · opus-5 62% · sonnet-5 38% · cache 96%`.
    ///
    /// Draws nothing at all when no row carries a `usage` object —
    /// ``Fleet/usageSummaryLine`` returns `nil` there, which is the case
    /// against an older proxy and against an offline read. A line of zeros
    /// would answer a question this build cannot answer, so silence is the
    /// honest output; every figure on this line is a measurement or it is
    /// absent. All of the arithmetic and the wording live on `Fleet`, where a
    /// test can assert on the finished string.
    ///
    /// Measured twice — as drawn, and as one line of the same font would be —
    /// so the height it takes when it wraps comes out of the account list's
    /// budget rather than out of the footer's place on screen. The baseline
    /// `Text` is hidden and drawn as an overlay, so it occupies no layout.
    /// This view only EMITS those two measurements; `header` observes them,
    /// outside this `if`, so that they reset when the line stops rendering.
    @ViewBuilder
    private var usageSummary: some View {
        if case .loaded(let fleet) = poller.state, let line = fleet.usageSummaryLine {
            Text(line)
                .font(Tok.secondaryDigitFont).lineSpacing(Tok.secondaryLineSpacing)
                .foregroundStyle(Tok.inkDim)
                .fixedSize(horizontal: false, vertical: true)
                .background(
                    GeometryReader { proxy in
                        Color.clear.preference(
                            key: UsageLineHeightKey.self,
                            value: proxy.size.height)
                    }
                )
                .overlay(alignment: .topLeading) {
                    Text(verbatim: "0")
                        .font(Tok.secondaryDigitFont).lineSpacing(Tok.secondaryLineSpacing)
                        .lineLimit(1)
                        .hidden()
                        .background(
                            GeometryReader { proxy in
                                Color.clear.preference(
                                    key: UsageLineBaselineKey.self,
                                    value: proxy.size.height)
                            }
                        )
                }
        }
    }

    /// Renders only for ``UpdateState/available(version:)`` and
    /// ``UpdateState/failed(_:)`` — see ``UpdateState/headerMessage``, which
    /// is what actually decides that; this view just draws whatever it
    /// returns. `.unknown` and `.upToDate` add no row: a permanent
    /// "you're up to date" line has no place in a 380pt panel. Same rule that
    /// drops the poll summary on a healthy read and moved `server <sha>` to the
    /// footer — a header line has to say something the next line does not.
    ///
    /// `.available` is a button so clicking it runs the same
    /// `checkForUpdates()` the footer's own button does — Sparkle drives the
    /// rest of the install flow from there, never this view.
    @ViewBuilder
    private var updateStateLine: some View {
        if let message = updater.updateState.headerMessage {
            let isFailure: Bool = {
                if case .failed = updater.updateState { return true }
                return false
            }()
            Button(message) { updater.checkForUpdates() }
                .buttonStyle(.plain)
                .font(Tok.secondaryFont).lineSpacing(Tok.secondaryLineSpacing)
                .foregroundStyle(isFailure ? Tok.spent : Tok.accent)
                .fixedSize(horizontal: false, vertical: true)
                // `.plain` draws no chrome, so the clickable region was the
                // glyph boxes themselves — about 14pt tall, and nothing at all
                // in the gaps between words. The padding and the explicit
                // content shape make the whole line one target.
                .padding(.vertical, Tok.space2)
                .contentShape(Rectangle())
        }
    }

    /// The one question the panel exists to answer: is there capacity right now,
    /// and if not, when does it come back. All counting lives on `Fleet`.
    ///
    /// The verdict and its tallies were two stacked lines and are now one that
    /// wraps, which gives the header a line back. Concatenated `Text` rather
    /// than an `HStack` because an `HStack` cannot wrap — it would overflow or
    /// truncate — while concatenated runs flow onto a second line and keep
    /// their own colours.
    private func capacitySummary(_ fleet: Fleet) -> some View {
        // v4-spec: summary line is 15pt/400 with numbers at 600 — literal
        // per the tokens-parity fence; `.subheadline`/`Tok.secondaryDigitFont`
        // were 13pt/11pt.
        var line =
            Text(fleet.capacitySummary)
            .font(Tok.nameFont)
            .foregroundColor(Tok.color(for: fleet.capacityState))
        for tally in fleet.breakdown {
            line =
                line
                + Text(" · ").font(Tok.summaryFont).foregroundColor(Tok.inkFaint)
                + Text(tally.label)
                .font(Tok.nameFont).monospacedDigit()
                .foregroundColor(Tok.color(for: tally.kind))
        }
        return
            line
            .fixedSize(horizontal: false, vertical: true)
            .accessibilityLabel("\(fleet.capacitySummary), \(fleet.breakdownLabel)")
            .padding(.top, Tok.tightSpacing)
            // `Text.lineSpacing` returns `Text` on its own but not once two
            // `Text` runs are concatenated with `+` (that operator is only
            // defined for `Text + Text`, and `.lineSpacing` here would make
            // the right side `some View`) — so this line, unlike every other
            // call site, takes `Tok.secondaryLineSpacing` once at the end
            // rather than per run. The lead run is drawn at `.subheadline`,
            // not a `Tok` size, so this is the same approximation the rest of
            // this token applies, carried one level further.
            .lineSpacing(Tok.secondaryLineSpacing)
            // Bound to the two facts that drive this line's colour, not the
            // summary TEXT: a count ticking should still read instantly, the
            // same as a digit always has. `capacityState` tints the lead run,
            // `breakdown` tints the tallies after it, and this line's height
            // never depends on either.
            .animation(reduceMotion ? nil : Tok.standardAnimation, value: fleet.capacityState)
            .animation(reduceMotion ? nil : Tok.standardAnimation, value: fleet.breakdown)
    }

    // MARK: Body

    @ViewBuilder
    private var content: some View {
        switch poller.state {
        case .pending:
            banner(
                icon: "clock",
                title: "Waiting for the first poll",
                detail: "Polling every \(Int(poller.interval))s.",
                tint: Tok.offline
            )
        case .toolMissing(let searched):
            banner(
                icon: Tok.unreadableGlyph,
                title: "tcr is not on PATH",
                detail: "Searched \(searched.count) locations. \(TcrTool.overrideRemedy)",
                tint: Tok.spent
            )
        case .commandFailed(let code, let message):
            banner(
                icon: Tok.unreadableGlyph,
                title: "tcr status failed (exit \(code))",
                detail: message.isEmpty ? "The server is probably not running." : message,
                tint: Tok.spent
            )
        case .undecodable(let message):
            // The decoder's own text is a Swift `DecodingError` description —
            // key paths and type names. It is the right thing to keep and the
            // wrong thing to lead with, so it moves to the tooltip.
            //
            // The body is the remedy alone, not a restatement: the header line
            // directly above already says what happened (`PollState.summary`),
            // and a banner that repeats the sentence above it is one the reader
            // has to check twice to find the new information in.
            banner(
                icon: Tok.unreadableGlyph,
                title: "Unreadable status output",
                detail: "Usually a version mismatch: update TcrBar, or the server.",
                tint: Tok.unknown
            )
            .help(message)
        case .loaded(let fleet):
            if fleet.accounts.isEmpty {
                banner(
                    icon: Tok.unreadableGlyph,
                    title: "No accounts configured",
                    detail: "tcr answered, and the fleet is empty. Run `tcr login` in a terminal to add one.",
                    tint: Tok.unknown
                )
            } else {
                fleetRows(fleet)
            }
        }
    }

    private func fleetRows(_ fleet: Fleet) -> some View {
        VStack(alignment: .leading, spacing: Tok.rowSpacing) {
            panelTabBar(fleet)
            switch selectedTab {
            case .accounts:
                if fleet.source.countersAreStructural {
                    offlineNotice(fleet.source)
                }
                if snapshotMode {
                    accountList(fleet)
                } else {
                    ScrollView {
                        accountList(fleet)
                    }
                    .frame(height: visibleRowsHeight(for: fleet))
                    .onPreferenceChange(RowHeightsKey.self) {
                        rowHeights = PanelHeight.settled(rowHeights, $0)
                    }
                }
            case .sessions:
                sessionsTab(fleet)
            case .tools:
                toolsTab(fleet)
            }
        }
    }

    // MARK: Tabs (F2 + F3)
    //
    // Only the Accounts tab measures its own rows (`RowHeightsKey`, above) —
    // Sessions and Tools deliberately do not. `visibleRowsHeight(for:)` is
    // driven entirely by `rowHeights`, which the Accounts tab keeps populated
    // the instant it is on screen (the default tab), so switching to either
    // new tab reuses that same number as a FIXED height budget rather than
    // measuring its own content. That is `panel-tabs-bridge.md`'s rule made
    // concrete: "Each tab gets a FIXED height budget through the same
    // functions; no tab's height may depend on content it renders" — the
    // popover's layout-cycle crash (`ffe8a86`) is exactly what a
    // content-dependent height on a tab switch would risk reopening.

    /// The segmented control at the top of the panel — SF Symbols
    /// `gauge`/`person.2`/`terminal`, a count badge on the two new tabs,
    /// mirroring `docs/design/panel-tabs-mockup.html`.
    private func panelTabBar(_ fleet: Fleet) -> some View {
        HStack(spacing: 3) {
            ForEach(PanelTab.allCases, id: \.self) { tab in
                tabButton(tab, badge: badge(for: tab, fleet: fleet))
            }
        }
        .padding(3)
        .background(RoundedRectangle(cornerRadius: Tok.radiusSmall + 2).fill(Tok.raised))
    }

    /// `nil` on `.accounts` (the mockup carries no badge on it either); a
    /// live session count on `.sessions`; the RUNNING count on `.tools` — the
    /// one figure a glance at the closed tab cannot otherwise see. Both zero
    /// out to `nil` rather than drawing a `0` badge, the same "a zero count
    /// should never render" rule `panel-tabs-bridge.md` names for the account
    /// cards.
    private func badge(for tab: PanelTab, fleet: Fleet) -> Int? {
        switch tab {
        case .accounts: return nil
        case .sessions: return fleet.sessions.isEmpty ? nil : fleet.sessions.count
        case .tools: return fleet.toolsRunning.isEmpty ? nil : fleet.toolsRunning.count
        }
    }

    /// Damping 1.0 (critically damped — settles without overshoot, i.e. "no
    /// bounce"), response ~0.3s. Local to the tab switch rather than
    /// `Tok.standardAnimation` (`.easeOut(duration: 0.15)`, tuned for a
    /// tint/parked-state crossfade elsewhere in this file and not owned by
    /// this lane) — a tab switch and a colour fade are different motions and
    /// sharing one token would make the next edit to either fight the other.
    /// `response`/`dampingFraction` rather than the newer `Spring` type: this
    /// package's deployment target is macOS 13 (`Package.swift`), and `Spring`
    /// needs macOS 14.
    private var tabSwitchAnimation: Animation {
        .spring(response: 0.3, dampingFraction: 1.0)
    }

    private func tabButton(_ tab: PanelTab, badge: Int?) -> some View {
        let isOn = tab == selectedTab
        return Button {
            if reduceMotion {
                selectedTab = tab
            } else {
                withAnimation(tabSwitchAnimation) { selectedTab = tab }
            }
        } label: {
            HStack(spacing: Tok.space2) {
                Image(systemName: tab.systemImage)
                Text(tab.title)
                if let badge {
                    Text("\(badge)")
                        .font(.caption2.weight(.semibold))
                        .contentTransition(.numericText())
                        .animation(reduceMotion ? nil : .easeOut(duration: 0.2), value: badge)
                        // v4-spec: tab badge 11pt on rgba(255,255,255,.14),
                        // radius 9, padding 0 6 — literal per the
                        // tokens-parity fence.
                        .font(.system(size: Tok.tabBadgeFontSize))
                        .padding(.horizontal, 6)
                        .background(Capsule().fill(Color.white.opacity(0.14)))
                }
            }
            // v4-spec: segmented tab label 12.5pt/600/+0.02em.
            .font(Tok.tabLabelFont)
            .tracking(0.02 * 12.5)
            .foregroundStyle(isOn ? Tok.ink : Tok.inkDim)
            .frame(maxWidth: .infinity)
            // v4-spec: item min-height 32, radius 8.
            .frame(minHeight: 32)
            .background(RoundedRectangle(cornerRadius: 8).fill(isOn ? Tok.hover : Color.clear))
        }
        .buttonStyle(.plain)
        .accessibilityLabel(badge.map { "\(tab.title), \($0)" } ?? tab.title)
    }

    /// One row per session, grouped by the account it is pinned to — an
    /// unpinned session (``Session/account`` `nil`) groups under
    /// "Unassigned", listed last.
    @ViewBuilder
    private func sessionsTab(_ fleet: Fleet) -> some View {
        if !fleet.sessionsSupported {
            banner(
                icon: "person.2",
                title: "No sessions yet",
                detail: "This server predates sessions — update tcr.",
                tint: Tok.unknown)
        } else if fleet.sessions.isEmpty {
            banner(
                icon: "person.2",
                title: "No live sessions",
                detail: "Nothing has spoken to the proxy in the last hour.",
                tint: Tok.inkDim)
        } else {
            let joined = SessionJoin.join(sessions: fleet.sessions, files: sessionFiles)
            if snapshotMode {
                sessionsList(joined)
            } else {
                ScrollView { sessionsList(joined) }
                    .frame(height: visibleRowsHeight(for: fleet))
            }
        }
    }

    private func sessionsList(_ sessions: [JoinedSession]) -> some View {
        let byAccount = Dictionary(grouping: sessions) { $0.session.account ?? "" }
        // Unassigned (`""`) sorts last; named accounts sort by name so the
        // list order does not reshuffle between two polls that carry
        // identical data.
        let order = byAccount.keys.sorted { lhs, rhs in
            if lhs.isEmpty != rhs.isEmpty { return rhs.isEmpty }
            return lhs < rhs
        }
        // The cap is across the WHOLE tab, not per account — the mockup's own
        // button counts that way ("Show 7 more sessions") — so the budget is
        // spent walking the accounts in display order and stops mid-account if
        // that is where the fifth session falls.
        let limit = sessionsExpanded ? Int.max : Self.sessionsVisibleRows
        var remaining = limit
        var visible: [(key: String, rows: [JoinedSession])] = []
        for key in order {
            guard remaining > 0 else { break }
            let rows = Array((byAccount[key] ?? []).prefix(remaining))
            remaining -= rows.count
            visible.append((key, rows))
        }
        let hidden = sessions.count - visible.reduce(0) { $0 + $1.rows.count }
        // `Tok.cardGap` — the same gap the Accounts tab's own list uses.
        return VStack(alignment: .leading, spacing: Tok.cardGap) {
            ForEach(visible, id: \.key) { entry in
                // v4-spec: each account's session block is a card, with the
                // account name as its first row and the sessions indented
                // under a 2px left rule.
                VStack(alignment: .leading, spacing: Tok.tightSpacing) {
                    HStack(spacing: Tok.tightSpacing) {
                        Text(entry.key.isEmpty ? "Unassigned" : entry.key)
                            // v4-spec: an account name is 15pt/600, the same
                            // value the Accounts tab's own name row uses.
                            .font(Tok.nameFont)
                            .foregroundStyle(Tok.ink)
                            .lineLimit(1)
                        Spacer(minLength: Tok.tightSpacing)
                        Text(sessionBlockSummary(entry.rows))
                            .font(Tok.dimLineFont)
                            .monospacedDigit()
                            .foregroundStyle(Tok.inkDim)
                            .lineLimit(1)
                    }
                    VStack(alignment: .leading, spacing: Tok.tightSpacing) {
                        ForEach(entry.rows) { row in
                            sessionRow(row)
                        }
                    }
                    // v4-spec "session block": padding-left 10, a 2px left
                    // rule in `line`.
                    .padding(.leading, 10)
                    .overlay(alignment: .leading) {
                        Rectangle().fill(Tok.hairlineStrong).frame(width: 2)
                    }
                }
                .panelCard()
            }
            if hidden > 0 {
                disclosureButton(
                    "Show \(hidden) more \(hidden == 1 ? "session" : "sessions")",
                    hint: "Shows every session the proxy has seen in the last hour."
                ) { toggleSessionsExpansion() }
            }
        }
    }

    /// "3 sessions · $9.41" — the trailing half of a session card's first row.
    ///
    /// The spend clause is dropped when not one session in the block was
    /// priced (``Session/costUsd`` is absent on any server built before wire
    /// 2), never printed as `$0.00`: the same rule the account cards' own
    /// spend line follows, through the same ``UsageTotals/addCost(_:_:)``.
    private func sessionBlockSummary(_ rows: [JoinedSession]) -> String {
        var spend: Double?
        for row in rows {
            spend = UsageTotals.addCost(spend, row.session.costUsd)
        }
        let noun = rows.count == 1 ? "session" : "sessions"
        guard let spend else { return "\(rows.count) \(noun)" }
        return "\(rows.count) \(noun) · \(QuotaFormat.usd(spend))"
    }

    /// An idle (or unknown-state) session draws ONE line — dot, name,
    /// subtitle, trailing "idle 40m" — never the second line's request
    /// count, cache percentage or sparkline. `docs/design/panel-tabs-mockup.html`'s
    /// `m-075377`/`token-b4` rows are exactly this shape: no metrics line at
    /// all under an idle session, where a busy or waiting one always has
    /// one. Splitting on that rather than always drawing both lines and
    /// hiding parts of the second is what keeps an idle row from claiming a
    /// sparkline and a cache reading nobody is currently producing.
    private var isCompactRow: (SessionActivity) -> Bool {
        { $0 == .idle || $0 == .unknown }
    }

    @ViewBuilder
    private func sessionRow(_ row: JoinedSession) -> some View {
        if isCompactRow(row.activity) {
            HStack(spacing: Tok.tightSpacing) {
                Circle()
                    .fill(activityColor(row.activity))
                    .frame(width: 8, height: 8)
                Text(row.displayName).font(.subheadline.weight(.semibold))
                Text(sessionSubtitle(row))
                    .font(Tok.dimLineFont)
                    .foregroundStyle(Tok.inkDim)
                    .lineLimit(1)
                Spacer()
                Text(trailingStatus(row, now: Date()))
                    .font(Tok.secondaryDigitFont)
                    .monospacedDigit()
                    .foregroundStyle(Tok.inkDim)
            }
            .padding(.vertical, Tok.space1)
        } else {
            VStack(alignment: .leading, spacing: Tok.space1) {
                HStack(spacing: Tok.tightSpacing) {
                    // Busy/waiting get a halo ring — `docs/design/panel-tabs-mockup.html`'s
                    // `.dot.busy`/`.dot.wait` box-shadow — the same "the
                    // color-alone trigger never fires" the review already
                    // credits this row for (the text beside it always says
                    // the state too).
                    Circle()
                        .fill(activityColor(row.activity))
                        .frame(width: 8, height: 8)
                        .background(
                            Circle().fill(Tok.halo(activityColor(row.activity)))
                                .frame(width: 16, height: 16)
                        )
                    Text(row.displayName).font(.subheadline.weight(.semibold))
                    Text(sessionSubtitle(row))
                        .font(Tok.dimLineFont)
                        .foregroundStyle(Tok.inkDim)
                        .lineLimit(1)
                    Spacer()
                    if let series = row.session.reqPerMinute, !series.isEmpty {
                        Sparkline(values: series)
                            .stroke(Tok.accent, lineWidth: 1.5)
                            .frame(width: 44, height: 14)
                    }
                }
                HStack(spacing: Tok.tightSpacing) {
                    Text("\(row.session.requests) req")
                        .monospacedDigit()
                        .contentTransition(.numericText())
                        .animation(
                            reduceMotion ? nil : .easeOut(duration: 0.2),
                            value: row.session.requests)
                    // `docs/design/panel-tabs-mockup.html`'s "$4.12" — the
                    // SERVER's own per-session figure (`SessionRow::cost_usd`,
                    // wire 2), never a price-per-token this app computes. No
                    // clause at all on a server that does not send it: the two
                    // earlier rounds of this bridge left the `$` out entirely
                    // for exactly that reason, and the field existing is what
                    // changed, not the rule.
                    if let cost = row.session.costUsd {
                        Text("· \(QuotaFormat.usd(cost))").monospacedDigit()
                    }
                    if let cache = cacheHitPercent(row.session) {
                        Text("· cache \(cache)%").monospacedDigit()
                    }
                    Spacer()
                    // A rolling digit transition here would need this string's
                    // VALUE, not its rendered text, to drive `.animation(value:)`
                    // — it is age-since-oldest-running-tool, recomputed from
                    // `Date()` on every render, so a state-driven transition has
                    // no discrete value to key off. Left as a plain `Text`.
                    Text(trailingStatus(row, now: Date())).monospacedDigit()
                }
                .font(Tok.secondaryDigitFont)
                .foregroundStyle(Tok.inkDim)
            }
            .padding(.vertical, Tok.space1)
        }
    }

    /// "teamclaude-rs · fable-5" — project and model on the session's title
    /// row, `docs/design/panel-tabs-mockup.html`'s `.sub`. Neither, one, or
    /// both may be missing (no session file, no reported model); this joins
    /// only what's present rather than drawing a bare "·". The model goes
    /// through ``QuotaFormat/modelLabel(_:)`` — the same `claude-` prefix
    /// and date-suffix strip the usage-by-model view already applies — so
    /// this row reads "opus-5", not the wire's "claude-opus-5", matching the
    /// mockup exactly instead of growing a second, subtly different
    /// shortener.
    private func sessionSubtitle(_ row: JoinedSession) -> String {
        [row.project, row.session.model.map(QuotaFormat.modelLabel)]
            .compactMap { $0 }.joined(separator: " · ")
    }

    /// `cacheReadTokens / (inputTokens + cacheReadTokens)`, the same ratio
    /// the account-level cache figure already uses elsewhere in this build.
    /// `nil` when the session has recorded no tokens at all yet, rather than
    /// a divide-by-zero "0%" that would claim a measured cold cache.
    private func cacheHitPercent(_ session: Session) -> Int? {
        let total = session.inputTokens + session.cacheReadTokens
        guard total > 0 else { return nil }
        return Int((Double(session.cacheReadTokens) / Double(total) * 100).rounded())
    }

    /// `docs/design/panel-tabs-review.md` finding 2: a session with a tool
    /// running shows the age of its OLDEST running tool, not its own
    /// `lastSeenMs` — a session cannot be "busy 3m" while a tool it owns has
    /// run for 9. This is also the fix for the finding's other half: reading
    /// `row.session.tools.running` here is the exact array
    /// ``Fleet/toolsRunning`` pools for the Tools tab, so the two tabs can no
    /// longer derive two different counts for the same session.
    private func trailingStatus(_ row: JoinedSession, now: Date) -> String {
        let running = row.session.tools.running
        guard !running.isEmpty, let oldestStartedMs = running.compactMap(\.startedMs).min() else {
            // `docs/design/panel-tabs-mockup.html`'s "waiting 12m" / "idle
            // 40m" — the activity word the dot already carries as colour,
            // said again in text so the row still reads without it (the
            // same "the color-alone trigger never fires" property every
            // other status on this tab already has). `.unknown` prints no
            // word: it is the join-has-no-file case, not a fourth state
            // this build has a name for, and inventing one here would claim
            // a reading nobody made.
            let age = row.ageLabel(now: now)
            switch row.activity {
            case .waiting: return "waiting \(age)"
            case .idle: return "idle \(age)"
            case .busy, .unknown: return age
            }
        }
        let elapsed = max(
            0, now.timeIntervalSince(Date(timeIntervalSince1970: Double(oldestStartedMs) / 1000)))
        return "\(running.count) running · oldest \(durationLabel(elapsed))"
    }

    private func activityColor(_ activity: SessionActivity) -> Color {
        switch activity {
        case .busy: return Tok.ok
        case .waiting: return Tok.near
        case .idle, .unknown: return Tok.inkFaint
        }
    }

    /// The ten slowest calls across every session, then running now, then
    /// fleet-wide totals — `panel-tabs-bridge.md`'s order for this tab.
    @ViewBuilder
    private func toolsTab(_ fleet: Fleet) -> some View {
        if !fleet.sessionsSupported {
            banner(
                icon: "terminal",
                title: "No tool data yet",
                detail: "This server predates sessions — update tcr.",
                tint: Tok.unknown)
        } else if snapshotMode {
            toolsList(fleet)
        } else {
            ScrollView { toolsList(fleet) }
                .frame(height: visibleRowsHeight(for: fleet))
        }
    }

    private func toolsList(_ fleet: Fleet) -> some View {
        // v4-spec: each section (RUNNING NOW, SLOWEST TODAY, BY TOOL) is a
        // card under its own section head, at the same `Tok.cardGap` the other
        // two tabs use — Gil, reading round 7's renders: "why sessions and
        // tools missing containers?". The head sits INSIDE its card here
        // rather than above it, so a section reads as one object; the mockup
        // draws it the same way.
        VStack(alignment: .leading, spacing: Tok.cardGap) {
            // `docs/design/panel-tabs-review.md` finding 3: the sum over
            // EVERY tool, never one category standing in for the total —
            // `toolsTotalCalls` already sums across every session's
            // `tools.calls`, whatever tool made each call.
            Text(
                "\(fleet.toolsTotalCalls) calls · \(fleet.toolsTotalErrors) errors"
                    + " · \(fleet.toolsTotalTimeouts) timeouts"
            )
            .font(Tok.secondaryDigitFont)
            .foregroundStyle(Tok.inkDim)

            if !fleet.toolsRunning.isEmpty {
                toolsSection("RUNNING NOW", subtitle: "Ring fills toward the 600s timeout") {
                    // Finding 5: state the denominator a ring fills toward,
                    // rather than drawing a fraction with no stated whole —
                    // that is the `subtitle` above.
                    ForEach(fleet.toolsRunning) { entry in runningToolRow(entry) }
                }
            }
            if !fleet.toolsSlowest.isEmpty {
                toolsSection("SLOWEST TODAY", subtitle: nil) {
                    ForEach(fleet.toolsSlowest) { entry in slowestToolRow(entry) }
                }
            }
            if fleet.toolsRunning.isEmpty && fleet.toolsSlowest.isEmpty {
                Text("No tool calls recorded yet.")
                    .font(Tok.dimLineFont)
                    .foregroundStyle(Tok.inkDim)
            }
            if let categories = fleet.toolsByCategory {
                toolsSection("BY TOOL", subtitle: "share of \(fleet.toolsTotalCalls)") {
                    ForEach(categories) { category in
                        byToolRow(category, total: fleet.toolsTotalCalls)
                    }
                }
            }
        }
    }

    /// One Tools-tab section: its head, an optional subtitle, and its rows,
    /// inside the panel's shared card box.
    ///
    /// The head lives inside the card rather than above it because the card IS
    /// the section — the mockup draws it that way, and a head floating over a
    /// box labels the gap as much as the box.
    private func toolsSection<Content: View>(
        _ title: String, subtitle: String?, @ViewBuilder rows: () -> Content
    ) -> some View {
        VStack(alignment: .leading, spacing: Tok.space1) {
            sectionHeading(title)
            if let subtitle {
                Text(subtitle)
                    .font(Tok.dimLineFont)
                    .foregroundStyle(Tok.inkFaint)
            }
            rows()
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .panelCard()
    }

    /// One `BY TOOL` bar — `docs/design/panel-tabs-mockup.html` F15: every
    /// bar is a share of the STATED total (never its own category's max), so
    /// the three widths are directly comparable at a glance.
    private func byToolRow(_ category: ToolCategory, total: Int) -> some View {
        let share = total > 0 ? Double(category.calls) / Double(total) : 0
        return HStack(spacing: Tok.tightSpacing) {
            Text(category.name)
                .font(Tok.dimLineFont)
                .foregroundStyle(Tok.inkDim)
                .frame(width: 84, alignment: .leading)
            GeometryReader { proxy in
                ZStack(alignment: .leading) {
                    Capsule().fill(Tok.hairline)
                    Capsule()
                        .fill(byToolColor(category.name))
                        .frame(width: max(2, proxy.size.width * share))
                }
            }
            .frame(height: 7)
            Text(byToolTrailingLabel(category))
                .font(Tok.dimLineFont)
                .foregroundStyle(Tok.inkDim)
                .lineLimit(1)
                .fixedSize()
        }
        .padding(.vertical, Tok.space1)
    }

    /// Bash is `--ok` green, Agent is `--info` blue (`.accent`, the closest
    /// token this palette has to the mockup's dedicated info role — see
    /// `docs/design/panel-tabs-review.md` finding 12, unresolved here), and
    /// Read/Grep/Edit is the neutral `--mute` — `docs/design/panel-tabs-mockup.html`'s
    /// three bar tints.
    private func byToolColor(_ category: String) -> Color {
        switch category {
        case "Bash": return Tok.ok
        case "Agent": return Tok.accent
        default: return Tok.inkFaint
        }
    }

    /// "19,913 · median 2.1s" — drops the median clause entirely when no
    /// session reported one, the same silence-over-a-guess rule as the rest
    /// of this tab.
    private func byToolTrailingLabel(_ category: ToolCategory) -> String {
        guard let median = category.medianSeconds else { return "\(category.calls)" }
        let formatted = median < 60 ? String(format: "%.1fs", median) : durationLabel(median)
        return "\(category.calls) · median \(formatted)"
    }

    private func sectionHeading(_ text: String) -> some View {
        // v4-spec: section head is 11pt/700/+0.1em uppercase — `.caption2` is
        // ~10pt; literal per the tokens-parity fence. The text itself is
        // already authored uppercase at every call site, so no `.uppercased()`
        // is added here.
        Text(text)
            .font(Tok.sectionHeadFont)
            .foregroundStyle(Tok.inkFaint)
            .tracking(0.1 * 11)
    }

    /// `commandHead` in monospace, truncated to one line — `panel-tabs-bridge.md`:
    /// "`command_head` shown in monospace, truncated to one line." Shared by
    /// ``runningToolRow(_:)`` and ``slowestToolRow(_:)`` so the two rows never
    /// drift on how a call names itself.
    private func toolCallLabel(_ entry: SessionToolEntry) -> some View {
        VStack(alignment: .leading, spacing: Tok.space1) {
            Text(entry.call.commandHead ?? entry.call.tool)
                .font(Tok.monoFont)
                .lineLimit(1)
                .truncationMode(.tail)
            Text("\(entry.call.tool) · \(toolCallOwnerName(entry.sessionId))")
                .font(Tok.dimLineFont)
                .foregroundStyle(Tok.inkDim)
        }
    }

    /// `docs/design/panel-tabs-mockup.html`'s "Bash · teamclaude-rs-c7" — the
    /// same ``JoinedSession/displayName`` the Sessions tab already draws for
    /// this session id, read off ``sessionFiles`` directly: `Fleet.toolsRunning`/
    /// `toolsSlowest` are pooled at the MODEL layer from the bare wire
    /// sessions and carry no file join (`SessionToolEntry` is `{sessionId,
    /// call}` only), so joining here — the one place this tab draws a
    /// session id — is cheaper than threading `SessionFile` through the
    /// model just to duplicate a lookup the Sessions tab already owns. Falls
    /// back to the id's first 8 characters, same as `JoinedSession`'s own
    /// rule, when no file matches.
    private func toolCallOwnerName(_ sessionId: String) -> String {
        if let name = sessionFiles[sessionId]?.name, !name.isEmpty { return name }
        return String(sessionId.prefix(8))
    }

    /// The Bash tool's own timeout — the one denominator this build knows,
    /// and the reason `docs/design/panel-tabs.md` names 600s specifically:
    /// "The ten slowest calls today all sit at 600s: the Bash tool's own
    /// timeout." A non-Bash running call has no known denominator and draws
    /// no ring, per finding 5's own fix ("state the denominator … set to the
    /// third ring") — inventing one for a tool this build has no timeout
    /// figure for would be the same overclaim finding 5 exists to remove.
    private let bashTimeoutSeconds: Double = 600

    /// A running call: its ring (Bash only, filling toward
    /// ``bashTimeoutSeconds``) and its elapsed time since `startedMs`, turning
    /// `--bad`/`Tok.spent` and naming the seconds left inside the last 20s —
    /// finding 5's fix, verbatim: "20s to timeout" beside the reddened ring.
    private func runningToolRow(_ entry: SessionToolEntry) -> some View {
        let now = Date()
        let elapsed = entry.call.startedMs.map { started in
            max(0, now.timeIntervalSince(Date(timeIntervalSince1970: Double(started) / 1000)))
        }
        let isBash = entry.call.tool == "Bash"
        let remaining = elapsed.map { bashTimeoutSeconds - $0 }
        let isNearTimeout = isBash && (remaining ?? .infinity) <= 20
        return HStack(spacing: Tok.tightSpacing) {
            toolCallLabel(entry)
            Spacer()
            VStack(alignment: .trailing, spacing: Tok.space1) {
                if isBash, let elapsed {
                    ZStack {
                        Circle().stroke(Tok.hairline, lineWidth: 3)
                        Circle()
                            .trim(from: 0, to: min(elapsed / bashTimeoutSeconds, 1))
                            .stroke(
                                isNearTimeout ? Tok.spent : Tok.ok,
                                style: StrokeStyle(lineWidth: 3, lineCap: .round)
                            )
                            .rotationEffect(.degrees(-90))
                    }
                    .frame(width: 18, height: 18)
                }
                if let elapsed {
                    Text(
                        isNearTimeout
                            ? "\(Int(max(0, remaining ?? 0)))s to timeout" : durationLabel(elapsed)
                    )
                    .font(Tok.secondaryDigitFont)
                    .monospacedDigit()
                    .foregroundStyle(isNearTimeout ? Tok.spent : Tok.inkDim)
                }
            }
        }
        .padding(.vertical, Tok.space1)
    }

    private func slowestToolRow(_ entry: SessionToolEntry) -> some View {
        HStack(spacing: Tok.tightSpacing) {
            toolCallLabel(entry)
            Spacer()
            if let seconds = entry.call.seconds {
                Text(durationLabel(seconds))
                    .font(Tok.secondaryDigitFont)
                    .monospacedDigit()
                    .foregroundStyle(seconds >= bashTimeoutSeconds ? Tok.spent : Tok.inkDim)
            }
        }
        .padding(.vertical, Tok.space1)
    }

    /// `"45s"`, `"4m 12s"` — no day tier: the longest call this tab shows is
    /// the Bash tool's own timeout, six orders of magnitude under a day.
    private func durationLabel(_ seconds: Double) -> String {
        let total = Int(seconds.rounded())
        let minutes = total / 60
        let rest = total % 60
        return minutes > 0 ? "\(minutes)m \(rest)s" : "\(rest)s"
    }

    /// "updated 4s ago" — `docs/design/panel-tabs-mockup.html` F14. No day/hour
    /// tier: a poll older than a few minutes is already covered by the
    /// non-healthy-read summary line above it in ``header``.
    private func freshnessLabel(since: Date, now: Date) -> String {
        let elapsed = max(0, now.timeIntervalSince(since))
        // Coordinator-flagged (2026-09-12): a fixture pinned to a fixed past
        // `referenceDate` (`RenderStates.referenceDate`) produced "updated
        // 54097m ago" against a real `Date()` — a five-digit minute count is
        // a defect regardless of what produced the gap, so this tiers all
        // the way up rather than assuming a poll is always recent.
        if elapsed < 60 { return "updated \(Int(elapsed.rounded()))s ago" }
        if elapsed < 3600 { return "updated \(Int((elapsed / 60).rounded()))m ago" }
        if elapsed < 86400 { return "updated \(Int((elapsed / 3600).rounded()))h ago" }
        return "updated \(Int((elapsed / 86400).rounded()))d ago"
    }

    /// Sessions tab summary — "12 sessions · 7 busy · 1 waiting · 4 idle",
    /// `docs/design/panel-tabs-mockup.html`'s Sessions panel. Joined with
    /// ``sessionFiles`` the same way ``sessionsList(_:)`` does, so the two
    /// counts on this tab (the summary and the rows below it) can never
    /// disagree about what "busy" means for a given session.
    private func sessionsSummaryLine(_ fleet: Fleet) -> some View {
        let joined = SessionJoin.join(sessions: fleet.sessions, files: sessionFiles)
        let busy = joined.filter { $0.activity == .busy }.count
        let waiting = joined.filter { $0.activity == .waiting }.count
        let idle = joined.count - busy - waiting
        var line = Text("\(joined.count) sessions").font(.subheadline.weight(.semibold))
        line =
            line + Text(" · ").foregroundColor(Tok.inkFaint)
            + Text("\(busy) busy").foregroundColor(Tok.ok)
        line =
            line + Text(" · ").foregroundColor(Tok.inkFaint)
            + Text("\(waiting) waiting").foregroundColor(Tok.near)
        line =
            line + Text(" · ").foregroundColor(Tok.inkFaint)
            + Text("\(idle) idle").foregroundColor(Tok.inkDim)
        return
            line
            .font(Tok.secondaryDigitFont)
            .fixedSize(horizontal: false, vertical: true)
            .padding(.top, Tok.tightSpacing)
            .lineSpacing(Tok.secondaryLineSpacing)
    }

    /// Tools tab summary — "24,677 tool calls today · 405 ran over 1m · 31 hit
    /// the 600s timeout", `docs/design/panel-tabs-mockup.html`'s Tools panel.
    /// `toolsTotalTimeouts` IS "hit the 600s timeout": the only tool this
    /// build tracks a timeout for is Bash, and its timeout is 600s
    /// (``bashTimeoutSeconds``), so a counted timeout and a 600s hit are the
    /// same event, not two facts that happen to agree.
    private func toolsSummaryLine(_ fleet: Fleet) -> some View {
        var line =
            Text("\(fleet.toolsTotalCalls) tool calls today").fontWeight(.semibold)
        if let overOneMinute = fleet.toolsOverOneMinute {
            line =
                line + Text(" · ").foregroundColor(Tok.inkFaint)
                + Text("\(overOneMinute) ran over 1m").foregroundColor(Tok.inkDim)
        }
        if fleet.toolsTotalTimeouts > 0 {
            line =
                line + Text(" · ").foregroundColor(Tok.inkFaint)
                + Text("\(fleet.toolsTotalTimeouts) hit the 600s timeout").foregroundColor(Tok.spent)
        }
        return
            line
            .font(Tok.secondaryDigitFont)
            .fixedSize(horizontal: false, vertical: true)
            .padding(.top, Tok.tightSpacing)
            .lineSpacing(Tok.secondaryLineSpacing)
    }

    /// The account list, cut into sections: a state band heading, then a group
    /// heading inside it, then the rows.
    ///
    /// This comment used to say a flat list was "the only shape this panel draws
    /// now", and cited `docs/plans/account-groups-plan.md:44-55` (decision \#4)
    /// for three rejected attempts at sectioning. **Gil has overridden that**,
    /// this session, knowing the cost. The objection was never that sectioning
    /// looked wrong — it was that a list cut by group must either duplicate a
    /// row for a multi-membership account or invent a "primary" group the data
    /// does not have. The override takes the first: an account in two groups
    /// gets TWO rows, one under each heading, and nothing here de-duplicates.
    /// The price is that the row count is no longer the account count and an
    /// operator will read the same email twice; the alternative was the app
    /// asserting which group an account "really" belongs to, which is a fact
    /// nobody has.
    ///
    /// Ordering, band membership and the duplication all live in
    /// ``Fleet/sectionsInDisplayOrder(pinning:)`` and are unit-tested there
    /// (`FleetSectionsTests`). Nothing is re-derived here: this function draws
    /// the array it is handed, in array order, and asks
    /// ``Array/isFirstOfBand(_:)`` when to open a band rather than comparing
    /// bands itself.
    ///
    /// The control account's hairline separator is GONE with the flat list that
    /// justified it. It marked the boundary under a globally pinned control row,
    /// and there is no such row any more — `sectionsInDisplayOrder` pins the
    /// control account to the front of each section it appears in, because
    /// hoisting it above the first band heading would put it outside every group
    /// and every state it belongs to. `visibleRowsHeight(for:)` therefore passes
    /// no `controlHairline` term: it existed to pay for a separator this list no
    /// longer draws, and charging for it now would make the viewport 8.5pt too
    /// TALL — the same drift the old comment warned about, in the other
    /// direction.
    private func accountList(_ fleet: Fleet) -> some View {
        let sections = fleet.sectionsInDisplayOrder(pinning: control.current)
        // v4-spec: gap between cards is 14pt — `Tok.cardGap` since #240,
        // which is that same value under the tokens lane's own name.
        // `Tok.rowSpacing` (8) is the gap between lines INSIDE a card.
        return VStack(alignment: .leading, spacing: Tok.cardGap) {
            // Identified by `FleetSection.id` (band + group), not by group
            // alone: a group whose accounts differ in state appears in more
            // than one band, and two sections sharing a SwiftUI identity paint
            // one section's rows into the other's slot.
            ForEach(Array(sections.enumerated()), id: \.element.id) { index, section in
                if drawsBandHeading(sections, at: index) {
                    bandHeading(section.band, soleGroupInBand: sections.soleGroupInBand(at: index))
                }
                sectionBody(
                    section, drawsHeading: drawsGroupHeading(sections, at: index), fleet: fleet)
            }
        }
    }

    /// One section's heading and rows, outlined in the group's colour when
    /// ``FleetSection/outlineColor`` resolves one (never for `.ungrouped`,
    /// and never for a named group with no resolved colour either — see
    /// that property's own doc-comment for why the two draw identically).
    ///
    /// `drawsHeading` is threaded through rather than recomputed, and
    /// `listChildKeys` below gates on the SAME call `accountList` made: a
    /// heading drawn but unkeyed is a row the viewport never charges for,
    /// and a heading keyed but not drawn charges for a row that is not
    /// there. Both clip.
    ///
    /// `FleetSectionRow` is `Identifiable` on the composite (group, account)
    /// id, which is why the inner `ForEach` can iterate `section.rows`
    /// directly: the duplicated account's two rows carry different
    /// identities.
    private func sectionBody(_ section: FleetSection, drawsHeading: Bool, fleet: Fleet)
        -> some View
    {
        // Two collapsed shapes, one expansion switch (`expandedGroups`).
        //
        // "maybe smoush the group" (Gil, 2026-09-12): a wholly-parked NAMED
        // group renders one line per account — no quota bars, no cost line —
        // and now stops after ``parkedVisibleRows`` of them with a "Show N
        // more accounts" button, `docs/design/panel-tabs-mockup.html`'s own
        // HENRY-TOKEN shape (3 of 5 visible).
        //
        // A LIVE group of four or more whose rows all still serve traffic
        // (``FleetSection/collapsesByDefault``) renders NO cards at all: one
        // summary line, its state tally, and a button. That is the mockup's
        // MYCELIUM group, and it is a real state rather than a clipped list —
        // `snapshotMode` draws it exactly as the live panel does.
        let expanded = expandedGroups.contains(section.group.token)
        let summarised = section.collapsesByDefault && !expanded
        let smooshed = section.isWhollyParked && !expanded
        let visibleParked = Array(section.rows.prefix(Self.parkedVisibleRows))
        let hiddenParked = section.rows.count - visibleParked.count
        return VStack(alignment: .leading, spacing: Tok.rowSpacing) {
            // The plain-text heading is for a section with no stroke to carry
            // a legend on — the unlabelled pile, or a named group whose
            // colour never resolved. An outlined section draws its name as
            // the legend in the `.overlay` below instead of here, never both.
            if drawsHeading && section.outlineColor == nil {
                groupHeading(section)
            }
            if summarised {
                measured(summaryHeightKey(section)) {
                    collapsedGroupSummary(section)
                }
            } else if smooshed {
                ForEach(visibleParked) { row in
                    measured(row.id) { smooshedAccountRow(row) }
                }
                if hiddenParked > 0 {
                    measured(disclosureHeightKey(section)) {
                        disclosureButton(
                            "Show \(hiddenParked) more \(hiddenParked == 1 ? "account" : "accounts")",
                            hint: "Shows the rest of this parked group."
                        ) { toggleGroupExpansion(section.group.token) }
                    }
                }
            } else {
                ForEach(section.rows) { row in
                    accountRow(row, fleet: fleet)
                }
            }
        }
        // Real padding now, not the old negative one — see `groupOutline`'s
        // own doc-comment for why. Zero for a section with nothing to
        // outline, so an unoutlined section's layout is byte-for-byte what
        // it was before this round.
        .padding(section.outlineColor == nil ? 0 : Tok.groupOutlineInset)
        .background(groupOutline(for: section))
        .overlay(alignment: .topLeading) {
            if section.outlineColor != nil {
                groupHeading(section)
                    .padding(.leading, Tok.space3)
                    .offset(y: -Tok.groupLegendHeight / 2)
            }
        }
    }

    /// How many accounts a collapsed, wholly-parked group still shows before
    /// the "Show N more accounts" button —
    /// `docs/design/panel-tabs-mockup.html`'s HENRY-TOKEN group draws three of
    /// its five. Three is enough for the group to read as a list of real
    /// accounts rather than as a number, and small enough that a large parked
    /// group cannot push the live ones off the panel.
    static let parkedVisibleRows = 3

    /// The whole of a collapsed LIVE group: "6 accounts · $8.42 today", its
    /// state tally, and the button that opens it.
    ///
    /// Everything drawn here is a ``FleetSection`` property — the line, the
    /// tally and the button's wording — so what an operator reads is testable
    /// without SwiftUI and cannot drift from what the section actually holds.
    private func collapsedGroupSummary(_ section: FleetSection) -> some View {
        VStack(alignment: .leading, spacing: Tok.space1) {
            HStack(spacing: Tok.tightSpacing) {
                // v4-spec: the dim line is 13pt/400. Tabular, like every
                // other changing number on this panel.
                Text(section.collapsedSummaryLine)
                    .font(Tok.dimLineFont)
                    .monospacedDigit()
                    .foregroundStyle(Tok.inkDim)
                    .lineLimit(1)
                Spacer(minLength: Tok.tightSpacing)
                ForEach(section.breakdown, id: \.kind.token) { tally in
                    StatusPill(tally.label, tint: tallyTint(tally.kind))
                }
            }
            disclosureButton(
                section.expandButtonLabel,
                hint: "Shows every account in this group."
            ) { toggleGroupExpansion(section.group.token) }
        }
    }

    /// The tint for one ``FleetTally/Kind``. `.needsRelogin` shares `spent`'s
    /// red because both mean the account serves nothing right now; the pill's
    /// own text is what distinguishes them, the same way every other status on
    /// this panel reads without its colour.
    private func tallyTint(_ kind: FleetTally.Kind) -> Color {
        switch kind {
        case .ok: return Tok.color(for: QuotaState.ok)
        case .near: return Tok.color(for: QuotaState.near)
        case .spent, .needsRelogin: return Tok.spent
        case .unknown: return Tok.unknown
        case .unmeasured: return Tok.unmeasured
        case .disabled: return Tok.disabled
        }
    }

    /// The one disclosure control on this panel — "Show 6 accounts in this
    /// group", "Show 2 more accounts", "Show 7 more sessions".
    ///
    /// v4-spec's button box (13pt label, radius 7, min-height 28, `line`
    /// border over a 10% white wash) drawn by hand rather than
    /// `.buttonStyle(.bordered)`: the system chrome is a capsule of a
    /// different height and cannot span the group's width, and this control's
    /// whole job is to be the width of the thing it opens.
    ///
    /// `hint` is spoken, never drawn — the label already says what the button
    /// shows, and a VoiceOver user gets the consequence instead of the same
    /// words twice.
    private func disclosureButton(
        _ title: String, hint: String, action: @escaping () -> Void
    ) -> some View {
        Button(action: action) {
            HStack(spacing: Tok.space1) {
                Image(systemName: "chevron.down")
                Text(title)
            }
            .font(Tok.buttonFont)
            .foregroundStyle(Tok.ink)
            .frame(maxWidth: .infinity, minHeight: Tok.buttonMinHeight)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .background(
            RoundedRectangle(cornerRadius: Tok.buttonRadius).fill(Tok.raised)
        )
        .overlay(
            RoundedRectangle(cornerRadius: Tok.buttonRadius)
                .strokeBorder(Tok.hairlineStrong, lineWidth: Tok.hairlineWidth)
        )
        .accessibilityHint(hint)
    }

    /// One line per account inside a collapsed, wholly-parked group: the
    /// name, its plan in dim text, and the SAME quota-state pill
    /// `AccountRow.information` draws — never its rotation pill, since every
    /// row here is parked by construction and the group's own legend already
    /// says so once for all of them. No quota bars and no cost line, on
    /// purpose: those are the two things this state exists to hide until the
    /// legend is clicked.
    private func smooshedAccountRow(_ row: FleetSectionRow) -> some View {
        let account = row.account
        return HStack(spacing: Tok.tightSpacing) {
            Text(account.name)
                .font(Tok.nameFont).lineSpacing(Tok.bodyLineSpacing)
                .foregroundStyle(account.disabled ? Tok.disabled : Tok.ink)
                .lineLimit(1)
                .help(account.name)
            if let plan = account.plan, !plan.isEmpty {
                Text(plan)
                    .font(Tok.muteLineFont).lineSpacing(Tok.detailLineSpacing)
                    .foregroundStyle(Tok.inkFaint)
                    .lineLimit(1)
            }
            Spacer(minLength: Tok.tightSpacing)
            smooshedStatePill(account)
        }
        .padding(.vertical, Tok.space1)
    }

    /// The quota-state pill for a smooshed row — the same five cases
    /// `AccountRow.information` draws its own quota pill from, mirrored here
    /// rather than shared because that property also reads `AccountRow`'s
    /// private `quotaTint` (which demotes a stale reading this collapsed
    /// view never shows a bar for in the first place).
    @ViewBuilder
    private func smooshedStatePill(_ account: Account) -> some View {
        if account.health == .needsRelogin {
            StatusPill("needs re-login", tint: Tok.spent)
        } else if account.isRejected {
            StatusPill("rejected", tint: Tok.spent)
        } else if account.hasQuotaEvidence {
            StatusPill(account.quotaState.token, tint: Tok.color(for: account.quotaState))
        } else if account.probeStatus.isFailure {
            StatusPill(account.probeStatus.token, tint: Tok.unmeasured)
        } else {
            StatusPill("unmeasured", tint: Tok.unmeasured)
        }
    }

    /// Draws NOTHING when ``FleetSection/outlineColor`` is `nil` — an
    /// earlier version of this drew a neutral ``Tok/hairlineStrong`` box for
    /// a named group with no resolved colour, mirroring ``GroupChip``'s own
    /// colourless-but-still-legible fallback. That does not transfer here: a
    /// chip stays informative without colour because it has text inside it;
    /// an outline IS the colour, so stripping it leaves a box that groups
    /// nothing while still competing for the eye — measured at 2.56:1 dark /
    /// 2.23:1 light against the panel, visible enough to be clutter and not
    /// visible enough to read as a boundary. An absent box beats an
    /// invisible one, so an unresolved colour now draws exactly like
    /// `.ungrouped`.
    ///
    /// The resolved colour itself is drawn at full strength, undimmed, even
    /// where its own measured contrast is low (`henry-team`'s green is
    /// 1.73:1 on the light panel) — that is reported, not silently
    /// corrected, because the group heading already names the group in
    /// text: the outline's colour is redundant decoration here, not
    /// information the reader has no other way to get, so it does not need
    /// to clear a text-legibility bar to be worth drawing.
    /// **Draws at the section's OWN bounds now, not past them.** The
    /// negative-padding version of this function bled the stroke outward
    /// into the surrounding `rowSpacing` gap so no real padding was ever
    /// added — cheap for `PanelHeight`, since nothing it measures grew, but
    /// the outward bleed only had room to survive vertically, where the gap
    /// between sections gave it somewhere to go. Horizontally there is no
    /// such gap: the account list sits in a fixed-width column, so the same
    /// bleed ran straight into that column's edge and was clipped —
    /// Gil, 2026-09-12: "the group doesnt do outline around all the group
    /// its just on top and under". `sectionBody` now insets its CONTENT by
    /// `Tok.groupOutlineInset` on all four sides instead, so this stroke —
    /// drawn unpadded, as a plain background — sits fully inside the padded
    /// box and is never asked to bleed past anything. The outer-equals-
    /// inner-plus-inset relationship `Tok.groupOutlineRadius`'s own doc
    /// comment describes still holds; only which side carries the inset
    /// changed.
    @ViewBuilder
    private func groupOutline(for section: FleetSection) -> some View {
        if let rgb = section.outlineColor {
            RoundedRectangle(cornerRadius: Tok.groupOutlineRadius)
                .strokeBorder(
                    Color(red: rgb.red, green: rgb.green, blue: rgb.blue),
                    lineWidth: Tok.groupOutlineWidth
                )
        }
    }

    /// The outer heading: what these accounts can do for you right now.
    /// ``FleetBand/title`` supplies the words — "Live", "Out of tokens",
    /// "Parked" — so the panel and any future caller cannot name a band two
    /// different ways.
    ///
    /// Drawn NOTHING when `soleGroupInBand` — see
    /// ``Array/soleGroupInBand(at:)``'s own doc-comment for why a band
    /// holding exactly one group section does not also get a band heading
    /// over it. Skipped rather than drawn-and-hidden, the same discipline
    /// ``groupHeading(_:)`` already follows: an unkeyed, undrawn heading
    /// charges the viewport nothing, and a keyed-but-undrawn one would
    /// charge for a row that is not there.
    /// Whether the list draws a band heading above the section at `index`.
    ///
    /// The draw loop and ``listChildKeys(_:)`` both ask THIS, not
    /// ``Array/isFirstOfBand(_:)`` directly: a heading drawn but unkeyed is a
    /// row the viewport never charges for, and a heading keyed but not drawn
    /// charges for a row that is not there. Both clip. `bandHeading` itself
    /// then makes the same two refusals (sole group in band, and the LIVE
    /// band) — kept there too because that is where each one's reason lives.
    private func drawsBandHeading(_ sections: [FleetSection], at index: Int) -> Bool {
        guard sections.isFirstOfBand(index), sections.indices.contains(index) else { return false }
        let section = sections[index]
        return !sections.soleGroupInBand(at: index) && section.band != .live
    }

    /// Whether the list draws a plain-text group heading above the section at
    /// `index` — ``Array/drawsGroupHeading(at:)``, minus the unlabelled pile.
    ///
    /// v4 draws no "Ungrouped" heading: the mockup's Accounts panel opens on
    /// the solo cards with nothing over them, and a heading whose whole
    /// content is "these have no group" is the row of dead space
    /// `drawsGroupHeading`'s own doc-comment already argues against for the
    /// single-section case. The model's rule is left alone — this is a
    /// drawing decision, and `FleetSectionsTests` pins the model's.
    private func drawsGroupHeading(_ sections: [FleetSection], at index: Int) -> Bool {
        guard sections.indices.contains(index) else { return false }
        return sections.drawsGroupHeading(at: index) && sections[index].group != .ungrouped
    }

    @ViewBuilder
    private func bandHeading(_ band: FleetBand, soleGroupInBand: Bool) -> some View {
        // The LIVE band draws no heading at all in v4 (Gil, reading the panel
        // beside `docs/design/panel-tabs-mockup.html`: the mockup's Accounts
        // panel opens straight onto the solo cards, and each group labels
        // itself with its own legend). `outOfTokens` and `parked` keep theirs:
        // the mockup never draws either state, so its silence is not evidence
        // for deleting the only label an ungrouped spent row has — a named
        // group carries "· PARKED" in its legend, an unlabelled pile carries
        // nothing.
        if !soleGroupInBand && band != .live {
            measured(bandHeightKey(band)) {
                Text(band.title.uppercased())
                    .font(Tok.sectionHeadFont).lineSpacing(Tok.detailLineSpacing)
                    .tracking(Tok.pillTracking)
                    .foregroundStyle(Tok.inkDim)
            }
        }
    }

    /// The inner heading: one group, or the unlabelled pile. Two shapes now:
    ///
    /// - Plain text, set apart from the band heading by case and weight
    ///   rather than by an indent (so a row and its heading keep the same
    ///   left edge), for a section with no outline to carry a legend on —
    ///   unchanged from before this round.
    /// - A LEGEND when ``FleetSection/outlineColor`` resolves one: the
    ///   group's name sitting ON its own stroke, in its own colour, small
    ///   caps with tracking — "why parked ontop of it and henry token group
    ///   name so on the side not some header" (Gil, 2026-09-12). This is
    ///   what replaces the old stacked band-heading-over-group-heading pair;
    ///   `sectionBody` draws it in an `.overlay`, never alongside the plain
    ///   text form. Reads " · PARKED" for a wholly-parked group — the one
    ///   place that word still survives once the per-row pill is dropped
    ///   (see `AccountRow.suppressParkedPill`) — and always " · N" for the
    ///   row count.
    ///
    ///   A wholly-parked group's legend is a real `Button`, not decorative
    ///   text with a tap gesture bolted on — `docs/design/panel-tabs-
    ///   review.md` finding 9 caught exactly that shape ("2 more · click to
    ///   expand", styled identically to the static text beside it) on the
    ///   sibling tabs mockup, and a `Text.onTapGesture` here would be the
    ///   same defect: no keyboard path, no announced role beyond whatever
    ///   `.accessibilityAddTraits` bolts on by hand. A live group's legend
    ///   stays plain `Text` — it has nothing to activate, and a control that
    ///   does nothing on activation is worse than no control (same review,
    ///   finding 1's whole premise).
    @ViewBuilder
    private func groupHeading(_ section: FleetSection) -> some View {
        if let rgb = section.outlineColor {
            let color = Color(red: rgb.red, green: rgb.green, blue: rgb.blue)
            // The mockup's own legend glyph — a rounded square in the group's
            // colour, `<rect rx=4>` at 11px, ahead of the name
            // (`docs/design/panel-tabs-mockup.html:291`). It is the second
            // channel the group's identity rides on: the outline is the same
            // hue and a reader who cannot separate two purples still gets a
            // mark tying legend to box.
            let legend = HStack(spacing: 4) {
                Image(systemName: "square.fill")
                    .font(.system(size: 8))
                Text(section.legendText)
                    .tracking(Tok.pillTracking)
            }
            .font(Tok.sectionHeadFont)
            .lineSpacing(Tok.detailLineSpacing)
            .foregroundStyle(color)
            .padding(.horizontal, Tok.space1)
            .padding(.vertical, Tok.space1)
            // Matches the panel body behind it, so the stroke line reads
            // as broken by the legend rather than running behind it.
            //
            // `Tok.panel` is a flat, fully opaque `Color` — this panel is
            // NOT a real translucent material (no `NSVisualEffectView`
            // anywhere in this target; `FleetView`'s own root is
            // `.background(Tok.panel)`, a plain fill). The tabs-mockup
            // review's finding 11 (an opaque legend patch reads as a
            // black bar over a backdrop-filter panel) is a defect in that
            // CSS approximation and does not reproduce here — checked
            // against both appearances in `--render-states`, scene
            // `15-parked-group`: the legend's background is pixel-
            // identical to the panel around it in both. Revisit this
            // comment the day this panel ever gains real vibrancy.
            .background(Tok.panel)
            if section.isWhollyParked || section.collapsesByDefault {
                Button {
                    toggleGroupExpansion(section.group.token)
                } label: {
                    legend
                }
                .buttonStyle(.plain)
                .accessibilityHint(
                    expandedGroups.contains(section.group.token)
                        ? "Collapses this group."
                        : "Expands this group."
                )
            } else {
                legend
            }
        } else {
            measured(groupHeightKey(section)) {
                Text(section.title)
                    .font(Tok.secondaryFont.weight(.semibold)).lineSpacing(Tok.secondaryLineSpacing)
                    .foregroundStyle(.primary)
            }
        }
    }

    /// Persisted across launches under ``expandedGroupsKey`` — a collapsed
    /// group an operator has already opened once should stay open, not
    /// re-collapse on the next poll or the next time the panel opens. Keyed on
    /// ``FleetGroupKey/token`` rather than the bare name for the same collision
    /// reason `FleetSectionRow.id` uses it: a group literally named
    /// `Ungrouped` cannot borrow the sentinel's key.
    ///
    /// ONE set for both collapsed shapes — the parked smoosh and a live
    /// group's summary line. "This group is open" is one fact about one group,
    /// and a second set keyed the same way would let the two disagree about a
    /// group that is parked today and live tomorrow.
    @State private var expandedGroups: Set<String> = Set(
        UserDefaults.standard.stringArray(forKey: FleetView.expandedGroupsKey) ?? [])

    /// Internal, not `private`, so ``RenderStates`` — this file's `--render-
    /// states` harness, same target — can seed and clear it around one scene
    /// (`15b-parked-group-expanded`) without a second copy of the literal.
    static let expandedGroupsKey = "expandedGroupNames"

    private func toggleGroupExpansion(_ groupToken: String) {
        if expandedGroups.contains(groupToken) {
            expandedGroups.remove(groupToken)
        } else {
            expandedGroups.insert(groupToken)
        }
        UserDefaults.standard.set(Array(expandedGroups), forKey: Self.expandedGroupsKey)
    }

    /// Whether the SESSIONS tab is showing every session or the first
    /// ``sessionsVisibleRows``. One flag for the whole tab, not one per
    /// account: the mockup's own button ("Show 7 more sessions") counts across
    /// accounts, and a per-account switch would leave a reader clicking five
    /// buttons to answer one question.
    @State private var sessionsExpanded: Bool = UserDefaults.standard.bool(
        forKey: FleetView.sessionsExpandedKey)

    static let sessionsExpandedKey = "sessionsListExpanded"

    /// How many session rows the Sessions tab draws before its own disclosure
    /// button — `docs/design/panel-tabs-mockup.html` shows five of twelve.
    static let sessionsVisibleRows = 5

    private func toggleSessionsExpansion() {
        sessionsExpanded.toggle()
        UserDefaults.standard.set(sessionsExpanded, forKey: Self.sessionsExpandedKey)
    }

    /// Height key for a collapsed group's summary block. Same `<letter>:`
    /// shape as every other key here, and `s:` cannot collide with a row key
    /// (those start with `g:`/`u:`) or with the band and heading keys.
    private func summaryHeightKey(_ section: FleetSection) -> String { "s:\(section.id)" }

    /// Height key for a disclosure button drawn under a section's rows.
    private func disclosureHeightKey(_ section: FleetSection) -> String { "d:\(section.id)" }

    /// Height key for a band heading. The `b:` prefix cannot collide with a row
    /// key — those start with a ``FleetGroupKey/token``, `g:` or `u:` — and the
    /// band's raw value rather than its title keeps it out of the operator's
    /// typing entirely.
    private func bandHeightKey(_ band: FleetBand) -> String { "b:\(band.rawValue)" }

    /// Height key for a group heading, built on ``FleetSection/id`` so a group
    /// split across two bands gets two keys, exactly as it gets two headings.
    private func groupHeightKey(_ section: FleetSection) -> String { "h:\(section.id)" }

    /// Publish a list child's measured height under `key`.
    ///
    /// Raw `proxy.size.height`, deliberately un-rounded: `PanelHeight.settled`
    /// is the only place the grid is applied, and a dead band that cannot see
    /// the raw value degenerates into the oscillation it exists to stop. Read
    /// its doc-comment before changing what is published here.
    private func measured<Content: View>(
        _ key: String,
        @ViewBuilder _ content: () -> Content
    ) -> some View {
        content().background(
            GeometryReader { proxy in
                Color.clear.preference(key: RowHeightsKey.self, value: [key: proxy.size.height])
            }
        )
    }

    private func accountRow(_ row: FleetSectionRow, fleet: Fleet) -> some View {
        let account = row.account
        // The group name this row is drawn under, if any — so the row can
        // suppress the one tag that would only restate the heading directly
        // above it. `nil` for the ungrouped pile, which carries no group name
        // to restate in the first place.
        let enclosingGroupName: String? = {
            if case .named(let name) = row.group { return name }
            return nil
        }()
        return AccountRow(
            account: account,
            countersAreStructural: fleet.source.countersAreStructural,
            accounts: accounts,
            control: control,
            onChanged: { await poller.pollOnce() },
            onRelogin: { reloginAccount(account.ref) },
            onMint: { mintAccountToken(account.ref) },
            onMintGroup: { group in mintGroupTokens(group) },
            groupController: groupController,
            removeController: removeController,
            allAccounts: fleet.accounts,
            enclosingGroupName: enclosingGroupName,
            // Every row reaching this section is parked BECAUSE this named
            // group is parked (`FleetSection.isWhollyParked`) — the group's
            // own legend already says so once, in text that survives without
            // colour, so restating it on every card under it is the same
            // fact twice in one glance. `false` (unchanged) for the
            // ungrouped pile and every live section.
            suppressParkedPill: row.band == .parked && row.group != .ungrouped,
            snapshotMode: snapshotMode
        )
        .background(
            GeometryReader { proxy in
                // `row.id`, never `account.id`: a duplicated account draws two
                // of these and `account.id` would make the second overwrite the
                // first's measurement.
                Color.clear.preference(
                    key: RowHeightsKey.self,
                    value: [row.id: proxy.size.height])
            }
        )
    }

    /// Height of the scroll viewport: every row, clamped to what the header
    /// left of `Tok.panelMaxHeight`.
    ///
    /// It used to stop at the first four rows, which left the panel short of its
    /// own cap while it had more to show — and a row count is the wrong unit
    /// anyway, since rows are not uniform height. A two-account fleet now draws
    /// in full and a thirteen-account one fills to 520pt and scrolls.
    ///
    /// `Tok.panelMaxHeight` is the cap on the LIST, and the panel is
    /// `header + Hairline + list + Hairline + footer`: a header that wraps to
    /// three lines used to add its extra height on top of that cap and push
    /// Quit off the bottom. The wrapped spend line's overflow is subtracted
    /// here instead — the cap itself is unchanged at 520, and a one-line header
    /// still gets the whole of it. The arithmetic is `PanelHeight`, where a
    /// test can run it.
    ///
    /// Fallback: before SwiftUI has laid out and reported any row height (the
    /// first frame, prior to the first `onPreferenceChange`), `rowHeights` is
    /// empty and this returns the budget itself, so the panel never renders at
    /// zero or one-row height while waiting for a real measurement.
    private func visibleRowsHeight(for fleet: Fleet) -> CGFloat {
        let sections = fleet.sectionsInDisplayOrder(pinning: control.current)
        let orderedHeights = listChildKeys(sections).compactMap { rowHeights[$0] }
        return PanelHeight.visibleRowsHeight(
            rowHeights: orderedHeights,
            spacing: Tok.rowSpacing,
            // No `controlHairline` term. It paid for the separator the flat
            // list drew under a globally pinned control row; sectioning removed
            // both the pin and the separator, so charging for it here would
            // hand the viewport 8.5pt of empty space under the last card.
            budget: PanelHeight.listBudget(
                cap: Tok.panelMaxHeight,
                headerOverflow: PanelHeight.headerOverflow(
                    lineHeight: usageLineHeight,
                    oneLineHeight: usageLineBaseline,
                    // The line's own render condition, read from the fleet
                    // rather than from the last measurement: a measurement
                    // taken while the line existed does not expire on its own.
                    lineIsDrawn: fleet.usageSummaryLine != nil),
                minimum: Tok.panelMinListHeight))
    }

    /// Every child `accountList` draws, in draw order, as its height key.
    ///
    /// One function for both jobs, which is the point: the drawing loop and the
    /// measuring sum have to agree about which children exist, and they used to
    /// drift over a single hairline (8.5pt of clipped card). Headings are
    /// children of the same `VStack` as the rows, so each one costs the viewport
    /// its own height plus one gap — and `PanelHeight.visibleRowsHeight` derives
    /// the gaps from this array's COUNT, so listing the headings here is what
    /// makes that arithmetic right rather than an approximation.
    ///
    /// A key with no measurement yet is dropped by the caller's `compactMap`,
    /// which is the same first-frame behaviour the rows have always had.
    private func listChildKeys(_ sections: [FleetSection]) -> [String] {
        var keys: [String] = []
        for (index, section) in sections.enumerated() {
            if drawsBandHeading(sections, at: index) {
                keys.append(bandHeightKey(section.band))
            }
            // Same predicate as the draw loop, deliberately. See there.
            if drawsGroupHeading(sections, at: index) {
                keys.append(groupHeightKey(section))
            }
            // Same three branches as `sectionBody`, in the same order and on
            // the same predicates — a collapsed group draws ONE block where
            // the expanded one draws a card per account, and charging the
            // viewport for six cards it is not drawing is how a panel ends up
            // with a scroll view taller than its own content.
            let expanded = expandedGroups.contains(section.group.token)
            if section.collapsesByDefault && !expanded {
                keys.append(summaryHeightKey(section))
            } else if section.isWhollyParked && !expanded {
                keys.append(contentsOf: section.rows.prefix(Self.parkedVisibleRows).map(\.id))
                if section.rows.count > Self.parkedVisibleRows {
                    keys.append(disclosureHeightKey(section))
                }
            } else {
                keys.append(contentsOf: section.rows.map(\.id))
            }
        }
        return keys
    }

    private func offlineNotice(_ source: StatusSource) -> some View {
        HStack(spacing: Tok.tightSpacing) {
            Image(systemName: "wifi.slash")
            Text("source: \(source.token) — quota is real, all serving counters are structurally zero.")
                .fixedSize(horizontal: false, vertical: true)
        }
        .font(Tok.secondaryFont).lineSpacing(Tok.secondaryLineSpacing)
        .foregroundStyle(Tok.offline)
    }

    private func banner(icon: String, title: String, detail: String, tint: Color) -> some View {
        HStack(alignment: .top, spacing: Tok.gutter) {
            Image(systemName: icon).foregroundStyle(tint)
            VStack(alignment: .leading, spacing: Tok.tightSpacing) {
                Text(title).font(.subheadline.weight(.semibold))
                Text(detail)
                    .font(Tok.secondaryFont).lineSpacing(Tok.secondaryLineSpacing)
                    .foregroundStyle(Tok.inkDim)
                    .fixedSize(horizontal: false, vertical: true)
            }
        }
        .padding(.vertical, Tok.tightSpacing)
    }

    // MARK: Footer

    private var footer: some View {
        VStack(alignment: .leading, spacing: Tok.tightSpacing) {
            // The running server's build, beside who supervises it — two facts
            // about the same process, neither about capacity. It used to sit in
            // the header, pushing the fleet's own numbers down for a string
            // read about once a release.
            HStack(spacing: Tok.tightSpacing) {
                // v4-spec: footer is 12.5pt/400, `--mute` throughout — both
                // runs were two different sizes/roles (`Tok.secondaryFont`
                // 11pt/dim, `Tok.detailDigitFont` 10pt/faint). Literal per
                // the tokens-parity fence.
                Text(server.state.summary)
                    .font(Tok.freshnessFont)
                    .foregroundStyle(Tok.inkFaint)
                    .fixedSize(horizontal: false, vertical: true)
                Spacer(minLength: Tok.tightSpacing)
                if case .loaded(let fleet) = poller.state, let sha = fleet.serverSha {
                    Text("server \(sha)\(fleet.serverDirty ? "-dirty" : "")")
                        .font(Tok.tabular(Tok.freshnessFont))
                        .foregroundStyle(Tok.inkFaint)
                        .lineLimit(1)
                }
            }
            fleetActions

            if let loginError {
                Text(loginError)
                    .font(Tok.detailFont).lineSpacing(Tok.detailLineSpacing)
                    .foregroundStyle(Tok.spent)
                    .fixedSize(horizontal: false, vertical: true)
            }

            // Start server at launch / Launch at login / Keep this Mac awake,
            // and Check for Updates / What's New / Quit, all moved to the
            // Settings window the gear opens (`data/plans/
            // settings-window-bridge.md`): "the panel loses the three
            // toggles, Check for Updates, What's New and Quit from its
            // bottom block; Refresh and Add account stay." ``fleetActions``
            // above is the surviving pair.
            dangerZone
        }
    }

    /// Actions that act on the FLEET: the proxy, the poll, the account list.
    ///
    /// Split from ``appActions`` because five bordered buttons do not fit.
    ///
    /// This was one row. The panel is `Tok.panelWidth` (380) wide with a
    /// `Tok.gutter` (12) on each side, so a row has **356pt**, and the five
    /// labels plus their bordered padding want roughly 460 — AppKit resolved
    /// that by truncating, and the row rendered as
    /// `Start se… · Refresh · Add ac… · Check fo… · Quit`. Three of the five no
    /// longer named their own action, and "Start se…" is not even unambiguous
    /// about its noun. A button whose label is cut is a button you have to click
    /// to identify.
    ///
    /// Note this was invisible to every test and to VoiceOver: SwiftUI truncates
    /// the DRAWN label and hands the full string to the accessibility layer, so
    /// the bug existed only for people looking at it. `--render-states` is what
    /// showed it, which is the harness working as intended.
    ///
    /// The split is by SCOPE, not by "what happened to fit". These three are
    /// about the fleet this panel monitors; the two below are about TcrBar
    /// itself — a distinction the footer already makes further down, where
    /// `launchAtLogin` is documented as living beside Quit precisely because it
    /// "is about TcrBar, not about the fleet".
    private var fleetActions: some View {
        HStack(spacing: Tok.tightSpacing) {
            if server.state.isOurChild {
                Button("Stop server") { server.stop() }
            } else {
                Button("Start server") { server.start() }
            }
            Button("Refresh") { Task { await poller.pollOnce() } }
            // Used to be disabled while a proxy served the port, enforcing a
            // `tcr login` refusal that stopped existing 2026-08-11 (`a385f0f`,
            // "feat: route tcr login through a live proxy instead of
            // refusing"): `login_route` (src/oauth.rs:966-1010) now probes the
            // running proxy and, when it is a modern build, routes the
            // finished credential *through* the server instead of refusing.
            // Gil runs a modern proxy, so the gate this button enforced no
            // longer applies to the only case that matters here — and a
            // disabled button in front of a working flow is a worse defect
            // than an occasional refusal from an older `tcr`.
            //
            // An older proxy still refuses, but it does so BEFORE any browser
            // opens, and its message names the remedy — which is useful only
            // if a human can read it, which is exactly what the Terminal
            // hand-off gives them.
            Button("Add account…") { addAccount() }
                .help(
                    "Opens `tcr login` in a Terminal window. It needs one: it "
                        + "prompts for a name and may ask for a pasted code. A "
                        + "modern proxy takes the login live even while serving; "
                        + "an older one refuses before any browser opens and "
                        + "prints how to recover."
                )
            Spacer(minLength: 0)
        }
        .buttonStyle(.bordered)
    }

    /// One message for all four `LoginLauncher` hand-offs below.
    ///
    /// They carried four identical copies of the same two-case switch, and the
    /// `toolMissing` copy named the problem with no way out of it — while the
    /// poll banner five hundred lines up already knew the remedy and gave it.
    /// `static` because it reads nothing from the view.
    private static func loginFailureMessage(_ why: LoginLauncher.Failure) -> String {
        switch why {
        case .toolMissing(let searched):
            return "tcr not found (searched \(searched.count) locations). "
                + TcrTool.overrideRemedy
        case .couldNotWriteScript(let message):
            return "Could not open Terminal: \(message)"
        }
    }

    /// Hand `tcr login` to a Terminal window.
    ///
    /// Deliberately a hand-off, not an in-app flow. `tcr login` refuses while a
    /// proxy holds the port and prompts on stdin, so a background spawn would
    /// usually fail and would hide its own prompts when it did not. The ellipsis
    /// in the label is doing real work: this opens something.
    private func addAccount() {
        if case .failure(let why) = LoginLauncher.launch() {
            loginError = Self.loginFailureMessage(why)
        } else {
            loginError = nil
        }
    }

    /// Same hand-off as ``addAccount()``, with the account name threaded
    /// through so the Terminal script can name it. Surfaces a failure the same
    /// way — a button that silently does nothing is worse than one that says
    /// why.
    private func reloginAccount(_ account: AccountRef) {
        if case .failure(let why) = LoginLauncher.launch(reloggingIn: account.name) {
            loginError = Self.loginFailureMessage(why)
        } else {
            loginError = nil
        }
    }

    /// Hand `tcr mint --account <name>` to a Terminal window for one row.
    ///
    /// Same hand-off reasoning as ``addAccount()``/``reloginAccount(_:)``, not
    /// ``TokenCommand``: `tcr mint` prompts on stdin once per account and
    /// puts the resulting token on the clipboard itself, so it needs a real
    /// terminal exactly like `tcr login` does, and unlike the one-shot `tcr
    /// token` whose entire stdout is the secret. Surfaces failure the same
    /// way — a button that silently does nothing is worse than one that says
    /// why — and never renders or logs a token.
    private func mintAccountToken(_ account: AccountRef) {
        if case .failure(let why) = LoginLauncher.launchMint(target: .account(account.name)) {
            loginError = Self.loginFailureMessage(why)
        } else {
            loginError = nil
        }
    }

    /// Same hand-off as ``mintAccountToken(_:)``, for every account carrying
    /// `group`'s label at once (`tcr mint --group <name>`).
    private func mintGroupTokens(_ group: String) {
        if case .failure(let why) = LoginLauncher.launchMint(target: .group(group)) {
            loginError = Self.loginFailureMessage(why)
        } else {
            loginError = nil
        }
    }

    /// Kept apart from the routine controls on purpose. "Refresh" costs nothing
    /// and "Take over port…" costs every live session a cold prompt-cache
    /// prefix; a misclick between neighbours is not an acceptable way to spend
    /// that, so this lives below its own rule, right-aligned, away from the row
    /// the hand is already in.
    private var dangerZone: some View {
        VStack(alignment: .leading, spacing: Tok.tightSpacing) {
            Hairline()
            HStack {
                appBuildTag
                Spacer()
                Button("Take over port…") { confirmTakeover() }
                    .buttonStyle(.bordered)
                    .foregroundStyle(Tok.spent)
                    // Disabled rather than silently no-op: the spawn path refuses
                    // a second child, so with one already supervised the click
                    // would do nothing and look like a failure.
                    .disabled(server.state.isOurChild)
                    .help(
                        server.state.isOurChild
                            ? "TcrBar already supervises a server — stop it first."
                            : "Replace the proxy currently holding the port. Expensive — asks first."
                    )
            }
        }
        .padding(.top, Tok.tightSpacing)
    }

    /// Which TcrBar this is, in the last line of the panel.
    ///
    /// The panel names the running proxy's build (`server <sha>`) because that
    /// process routinely lags its source. TcrBar has the same gap and, until
    /// now, no answer for it: the bundle cannot be replaced while it runs, so
    /// an update that looks installed can leave the old app on screen — and the
    /// only way to find out was to quit and read About. ``AppBuild`` reads the
    /// keys `scripts/build-tcrbar.sh` has always written.
    ///
    /// It rides the danger row rather than adding a line of its own because
    /// that row is one right-aligned button with 250pt of empty space beside
    /// it, and the panel's height is already the scarce thing here — this is
    /// the one place a fact fits at no cost. It is not part of that group and
    /// must not read as part of it, so it carries `.tertiary` and the detail
    /// font, matching `server <sha>` above; every control in the danger group
    /// is `Tok.spent`. Drawn last so it sits at the very bottom, which is where
    /// a version tag is looked for.
    ///
    /// Absent rather than approximate: `swift run TcrBar` has no Info.plist, so
    /// there is no version, so nothing is drawn.
    @ViewBuilder
    private var appBuildTag: some View {
        if let label = AppBuild.label {
            Text(label)
                .font(Tok.detailDigitFont).lineSpacing(Tok.detailLineSpacing)
                .foregroundStyle(Tok.inkFaint)
                .lineLimit(1)
                .textSelection(.enabled)
                .help(AppBuild.buildDetail(buildNumber: AppBuild.buildNumber) ?? label)
        }
    }

    /// The alert names the real cost in plain language, defaults to Cancel, and
    /// styles the other button as destructive. Only on an explicit confirm does
    /// this call `startTakingOverPort()`, which spawns
    /// `tcr server --headless --replace`
    /// — the replacement is performed by `tcr`'s own singleton.
    /// TcrBar signals nothing it did not spawn.
    private func confirmTakeover() {
        let alert = NSAlert()
        alert.alertStyle = .critical
        alert.messageText = "Take over the port from the running proxy?"
        alert.informativeText = """
            This kills the proxy currently holding the port and starts a new one \
            in its place.

            That wipes the session-to-account pin map. Anthropic's prompt cache is \
            per-account, so every live Claude session is re-pinned to a different \
            account and pays a full cold prompt-cache prefix on its next request. \
            It is the most expensive thing this app can do.

            Only do this if the running proxy is stuck or is a build you need to \
            replace.
            """
        let takeOver = alert.addButton(withTitle: "Take over port")
        takeOver.hasDestructiveAction = true
        alert.addButton(withTitle: "Cancel")
        // Cancel is the default: Return and Escape both back out.
        alert.window.defaultButtonCell = nil
        alert.buttons.last?.keyEquivalent = "\r"
        takeOver.keyEquivalent = ""

        guard alert.runModal() == .alertFirstButtonReturn else { return }
        server.startTakingOverPort()
    }
}

/// Carries the measured height of each list child out of the scroll view,
/// keyed by that child's list identity — ``FleetSectionRow/id`` for a row, the
/// `b:`/`h:` heading keys for a band or group heading. **Not `Account.id`**: a
/// multi-group account draws two rows, and two rows publishing one key means
/// one measurement lands and the other is silently discarded.
///
/// A dictionary rather than one summed scalar because rows are not uniform
/// height — `visibleRowsHeight(for:)` needs the first N individually, in
/// display order, not just their total.
/// The panel's three tabs (F2 + F3, `data/plans/panel-tabs-bridge.md`).
/// `CaseIterable` so `panelTabBar` draws them in one declared order rather
/// than a second list a reviewer has to keep in sync with this one.
enum PanelTab: Equatable, CaseIterable {
    case accounts, sessions, tools

    var title: String {
        switch self {
        case .accounts: return "Accounts"
        case .sessions: return "Sessions"
        case .tools: return "Tools"
        }
    }

    var systemImage: String {
        switch self {
        case .accounts: return "gauge"
        case .sessions: return "person.2"
        case .tools: return "terminal"
        }
    }
}

/// A session row's req/min trend — `docs/design/panel-tabs-mockup.html`'s
/// `.spark` polyline. Draws `nil`/negative values as zero rather than
/// crashing the normalisation on an empty range; a flat all-zero series
/// still draws a flat line at the bottom, which is the honest shape for
/// "measured, and nothing happened."
struct Sparkline: Shape {
    let values: [UInt16]

    func path(in rect: CGRect) -> Path {
        var path = Path()
        guard values.count > 1 else { return path }
        let maxValue = max(1, Double(values.max() ?? 1))
        let stepX = rect.width / CGFloat(values.count - 1)
        func point(_ index: Int) -> CGPoint {
            let x = CGFloat(index) * stepX
            let y = rect.height * (1 - CGFloat(Double(values[index]) / maxValue))
            return CGPoint(x: x, y: y)
        }
        path.move(to: point(0))
        for i in 1..<values.count { path.addLine(to: point(i)) }
        return path
    }
}

struct RowHeightsKey: PreferenceKey {
    static let defaultValue: [String: CGFloat] = [:]
    static func reduce(value: inout [String: CGFloat], nextValue: () -> [String: CGFloat]) {
        value.merge(nextValue()) { _, new in new }
    }
}

/// The spend line's drawn height, and the height of one line of it — the pair
/// `PanelHeight.listBudget` turns into the header's overflow. Same
/// preference-key pattern the rows use, for the same reason: the wrap point
/// depends on the font, the panel width and the string, so it is measured, not
/// computed.
struct UsageLineHeightKey: PreferenceKey {
    static let defaultValue: CGFloat = 0
    static func reduce(value: inout CGFloat, nextValue: () -> CGFloat) {
        value = max(value, nextValue())
    }
}

struct UsageLineBaselineKey: PreferenceKey {
    static let defaultValue: CGFloat = 0
    static func reduce(value: inout CGFloat, nextValue: () -> CGFloat) {
        value = max(value, nextValue())
    }
}

extension View {
    /// The panel's one card box — v4-spec's `.card`: radius 8, padding 10 12,
    /// the raised surface fill and a 1px `line` border.
    ///
    /// One definition for all three tabs. The Accounts tab had it inline on
    /// ``AccountRow``; Sessions and Tools drew their content flat against the
    /// panel, which is the gap Gil read off round 7's renders ("why sessions
    /// and tools missing containers?"). Writing a second, near-identical box
    /// for them would have been two places to change a radius.
    ///
    /// The radius stays a literal 8 after `feat/tokens-parity` landed (#240),
    /// and that is deliberate: its `Tok.cardRadius` is an alias for
    /// `radiusMedium`, 16, while `data/plans/v4-spec.md` measures the mockup's
    /// card at 8. Swapping the name in would double the curve this bridge
    /// exists to close. The gap between cards DID move to `Tok.cardGap` (14),
    /// which agrees with the sheet. Revisit when `feat/tokens-v4` restyles the
    /// radius family.
    func panelCard() -> some View {
        self
            .padding(.horizontal, 12)
            .padding(.vertical, 10)
            .background(
                RoundedRectangle(cornerRadius: 8).fill(Tok.raised)
            )
            .overlay(
                RoundedRectangle(cornerRadius: 8)
                    .strokeBorder(Tok.hairlineStrong, lineWidth: Tok.hairlineWidth)
            )
    }
}

/// One account.
///
/// The enable/disable control is a `tcr` subprocess and nothing else: this app
/// never touches `~/.config/teamclaude.json`, which holds credentials. The row
/// does not optimistically flip its own `disabled` — on success it asks for a
/// fresh poll and renders whatever `tcr status` then reports, so a call that
/// resolved to some other account shows up as the wrong row changing rather than
/// as a lie. That is a backstop, not the expectation: resolution is an exact
/// name match falling back to an exact email match, case-sensitive both times
/// (`src/identity.rs`, `match_accounts`), and two accounts sharing an email
/// across orgs come back ambiguous rather than resolved. A non-zero exit is
/// rendered in place.
///
/// This row no longer lets a toggle succeed silently. `tcr` rewrites the config
/// and exits 0; a proxy that read `disabled` once at boot keeps reporting the old
/// value, and `tcr status` prefers the live server — so the refresh used to come
/// back unchanged and the row showed nothing at all. Every toggle now ends in a
/// stated outcome from ``ToggleVerdict``, drawn by `verdictLine` and spoken by
/// `rowAccessibilityLabel`.
///
/// No confirmation dialog: disabling is reversible and costs nothing.
struct AccountRow: View {
    let account: Account
    let countersAreStructural: Bool
    @ObservedObject var accounts: AccountController
    /// The identity-bound control account. See ``ControlAccountController``.
    @ObservedObject var control: ControlAccountController
    /// Called after a successful toggle so the row re-reads reality. It returns
    /// the state of the read it just performed, which is what the verdict is
    /// computed against — reading the poller's published `state` afterwards could
    /// pick up a different, later poll.
    let onChanged: () async -> PollState
    /// Hands `tcr login` to a Terminal window for THIS account. Drawn on
    /// every row's context menu, and additionally as the standalone
    /// ``AccountRow/reloginButton`` on a `.needsRelogin` row.
    let onRelogin: () -> Void
    /// Hands `tcr mint --account <this row's name>` to a Terminal window. See
    /// ``FleetView/mintAccountToken(_:)``.
    let onMint: () -> Void
    /// Hands `tcr mint --group <name>` to a Terminal window for every account
    /// carrying that label — the group-level twin of ``onMint``, taking the
    /// group name since this row's own account is not necessarily the one
    /// whose tokens end up minted. See ``FleetView/mintGroupTokens(_:)``.
    let onMintGroup: (String) -> Void
    /// Mutating THIS row's own group membership from its context menu — the
    /// affordance the bridge specifically asked for: before this round,
    /// membership could only be changed from a section-header menu, and the
    /// instinct is to act on the account itself. See ``groupMenuItems``.
    @ObservedObject var groupController: GroupController
    /// Deleting this row's account. See ``RemoveAccountController``.
    @ObservedObject var removeController: RemoveAccountController
    /// The whole fleet, so ``addToGroupMenu`` can offer every group this
    /// account is not already in, not just the ones visible in whatever
    /// section this row happens to be drawn under.
    let allAccounts: [Account]
    /// The name of the group section this row is currently drawn under, or
    /// `nil` for the ungrouped pile. Used only to suppress that ONE tag in
    /// ``designationsLine`` — see its doc-comment — never to filter or reorder
    /// ``Account/groupTags`` itself, which still lists every group this
    /// account belongs to.
    var enclosingGroupName: String?
    /// True when this row sits inside a wholly-parked NAMED group section
    /// (``FleetSection/isWhollyParked``) — every row there is parked for the
    /// same reason the group heading now names as a legend, so
    /// ``rotationPill`` drops the per-row "parked" pill it would otherwise
    /// draw. Defaults to `false`, unchanged for every other call site.
    var suppressParkedPill: Bool = false
    /// Mirrors ``FleetView/snapshotMode``. `ImageRenderer` cannot draw a
    /// `Menu` — it rasterises the yellow "unsupported control" placeholder
    /// the README hero used to ship — so a snapshot draws
    /// ``accountActionsMenuLabel`` on its own, with no `Menu` wrapping it.
    /// Defaults to `false` so every other call site (the live panel) is
    /// unchanged.
    var snapshotMode: Bool = false
    /// The last "Copy Access Token" that did not land — `tcr`'s own words,
    /// drawn under the row like the other failure lines. Row-local rather
    /// than a controller: nothing else needs to know, and a copy that did
    /// land leaves no state at all (the pasteboard is the only evidence).
    @State private var tokenCopyFailure: TokenCommand.Failure?

    /// Honour the system Reduce Motion setting, the way ``QuotaBar`` does.
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    /// The single tint for this row's quota evidence. The bar and the
    /// percentage run both read it, so the two can never disagree about
    /// whether a quota is known — or, now, about whether it is still LIVE.
    ///
    /// `Tok.unmeasured`, not `Tok.unknown`: `FleetTally.Kind` documents these as
    /// deliberately separate — "a quota state this build cannot name" is not the
    /// same fact as "no quota state at all", and colouring them alike re-merges
    /// exactly what the optional model exists to keep apart.
    ///
    /// `.needsRelogin` is checked FIRST, ahead of `hasQuotaEvidence`, for a
    /// reason that is not cosmetic. A broken account that died AFTER being
    /// probed keeps its last-learned `quota` and `quotaState` — often `.ok` —
    /// so `Tok.color(for: .ok)` would draw the SAME green a genuinely healthy
    /// row draws. Green in this palette means "you can work right now"; this
    /// account's apparent headroom is unreachable until re-login
    /// (`src/manager/select.rs:931` sends it nothing), so drawing it green
    /// overclaims exactly the way an unfilled bar overclaims for a nil
    /// reading — the same reason `QuotaBar`'s own doc-comment makes a nil
    /// draw DASHED rather than empty. Stale-versus-live is that distinction
    /// one step over, and this row already committed to the principle.
    ///
    /// Neither existing hue fits the stale case. `Tok.unmeasured` is spoken
    /// for by "never probed" — reusing it re-merges the two causes this
    /// branch spent three rounds separating. Dashed/`Tok.unknown` would claim
    /// no reading exists, which misattributes the cause exactly like the grey
    /// `unmeasured` pill did before this branch started. Red (`Tok.spent`,
    /// already worn by the pill and the status word on this row) would claim
    /// the quota is EXHAUSTED, a different and equally false fact — the
    /// number is real and is not zero. `Tok.disabled` is the closest existing
    /// token in MEANING ("this row is not in play") even though its literal
    /// cause (an operator's own choice) is not this row's cause; it is reused
    /// rather than adding a colour, since this repo's palette is generated
    /// and gated on WCAG contrast, and a new token is a heavier change than
    /// muting a stale reading needs.
    private var quotaTint: Color {
        if hasStaleQuotaReading { return Tok.disabled }
        return account.hasQuotaEvidence ? Tok.color(for: account.quotaState) : Tok.unmeasured
    }

    /// The 5-hour bar's OWN tint — Gil's explicit call: a 7d-red account with an
    /// empty 5h window must not paint its 5h bar red.
    ///
    /// Routes through `Account.quotaBarTintSource(for:)`
    /// (`TcrBarCore/FleetStatus.swift`) rather than reading
    /// `effectiveQuotaState(for:)` directly — that was the bug this comment
    /// used to describe incorrectly: `effectiveQuotaState` alone cannot tell
    /// "this window has no reading" from "old server, borrow the composite
    /// state", so a naive call fell through to the COMPOSITE (7d-driven)
    /// state and painted an empty 5h bar red whenever the 7d window was
    /// spent — the exact overclaim two-window tinting exists to prevent, and
    /// an ordinary state, not an exotic one (`src/quota.rs` populates the two
    /// windows independently from separate response headers, so one sitting
    /// at `None` while its sibling accumulates happens routinely). Proven
    /// with the `01d-unmeasured-window-proof` golden scene before the fix
    /// landed: the 5h outline rendered red. `quotaBarTintSource` makes the
    /// no-reading-vs-old-server distinction correctly (gates on the
    /// FRACTION, not the state word — see its own doc-comment) and is pinned
    /// by `QuotaWindowStateTests`.
    ///
    /// `hasStaleQuotaReading` demotes BOTH bars together, same as `quotaTint`.
    private var fiveHourTint: Color {
        if hasStaleQuotaReading { return Tok.disabled }
        switch account.quotaBarTintSource(for: .fiveHour) {
        case .unmeasured: return Tok.unmeasured
        case .state(let state): return Tok.color(for: state)
        }
    }

    /// The 7-day bar's own tint — the 5h counterpart to ``fiveHourTint``, same
    /// `quotaBarTintSource` routing and same stale-demotion rule.
    private var sevenDayTint: Color {
        if hasStaleQuotaReading { return Tok.disabled }
        switch account.quotaBarTintSource(for: .sevenDay) {
        case .unmeasured: return Tok.unmeasured
        case .state(let state): return Tok.color(for: state)
        }
    }

    /// What colour the reset caption wears.
    ///
    /// `Tok.inkFaint` while the window is fine — the percentage beside it is the
    /// fact, and the countdown must not compete with it. On a `near` or `spent`
    /// window it takes that window's colour, because there the countdown IS the
    /// fact: it answers when capacity comes back, and it should be findable down
    /// a column of thirteen rows.
    ///
    /// Demoted with the bar and the percentage on a stale reading.
    private func captionTint(for window: Account.QuotaWindow) -> Color {
        if hasStaleQuotaReading { return Tok.disabled }
        guard case .state(let state) = account.quotaBarTintSource(for: window) else {
            return Tok.inkFaint
        }
        switch state {
        case .near, .spent: return Tok.color(for: state)
        case .ok, .unknown: return Tok.inkFaint
        }
    }

    /// True for exactly the shape this whole round exists to demote: a
    /// broken account that has a REAL last-learned quota reading, not an
    /// absent one. Gated on `hasQuotaEvidence` as well as `health`, not
    /// `health` alone — a broken account that was NEVER probed (the `04b`
    /// scene) has nothing filled to overclaim with: its bar is already the
    /// dashed "no reading" outline, unambiguous regardless of colour, and
    /// its percentage already reads "n/a". Recolouring either would be a
    /// change with nothing behind it, and `04b` stays byte-identical because
    /// this stays false for it. Shared by `quotaTint`, the bar's `.help`, and
    /// the two percentage runs, so all four demote together or not at all.
    private var hasStaleQuotaReading: Bool {
        account.health == .needsRelogin && account.hasQuotaEvidence
    }

    /// The 5h line's spend figures, demoted with everything else on the row
    /// when the reading is historical.
    ///
    /// `Color`, not `AnyShapeStyle`. This reached for SwiftUI's `.tertiary`,
    /// which has no `Color` spelling and forced the erasure — and which drew
    /// this figure at 1.86:1 on the light panel, measured off a
    /// `--render-states` bitmap. A hierarchical style resolves against the
    /// SYSTEM window background, never against `Tok.raised`, so none of the
    /// palette's measured ratios ever applied to it. `Tok.inkFaint` is this
    /// panel's own tertiary ink and `scripts/tcrbar-palette.py` gates it at
    /// 4.5:1 against both surfaces.
    private var usageFigureStyle: Color {
        hasStaleQuotaReading ? Tok.disabled : Tok.inkFaint
    }

    /// The Fable weekly figure's tint, from that window's OWN state.
    ///
    /// It carries the colour because it has no bar to carry it — the 7d line's
    /// own percentage can stay neutral beside a tinted bar, and this one cannot.
    /// `near` and `spent` take the window's hue for the same reason
    /// `captionTint(for:)` does: there the figure IS the news, and it should be
    /// findable down a column of thirteen rows. `ok` stays neutral rather than
    /// green, because that is twelve of those thirteen rows and colouring the
    /// unremarkable case spends the panel's colour budget on it.
    ///
    /// An ABSENT `sevenDayOiState` is neutral too, and deliberately does not
    /// fall back to the composite `quotaState` the way `effectiveQuotaState`
    /// does for the other two windows: the composite is the most-spent of all
    /// three, so borrowing it paints the Fable figure red because the 5-hour
    /// window is spent. That is not a hypothetical — `sevenDayOi` has been on
    /// the wire far longer than `sevenDayOiState`, so a live proxy sends the
    /// fraction with no state beside it routinely. The percentage is still a
    /// measurement and still prints; only the state colour is withheld.
    ///
    /// Demoted with the rest of the row on a stale reading, like every other
    /// figure on it.
    ///
    /// `Color` for the same reason as ``usageFigureStyle``: every branch is a
    /// token now, so nothing needs erasing.
    private var fableFigureStyle: Color {
        if hasStaleQuotaReading { return Tok.disabled }
        guard let state = account.sevenDayOiState else { return Tok.inkFaint }
        switch state {
        case .near, .spent: return Tok.color(for: state)
        case .ok, .unknown: return Tok.inkFaint
        }
    }

    /// Whether this account is in the rotation, said in BOTH directions.
    ///
    /// This row used to render a pill only when `disabled` was true, so "in
    /// rotation" was signalled by the absence of anything — and the only nearby
    /// text was a button reading "Disable", which names what a click would DO.
    /// That is not a null state, it is a legible one: the button's verb was read
    /// as a status, and the reader concluded from it that the proxy was routing
    /// traffic to a disabled account. It was not; `disabled` was false. A row
    /// that can be misread as its own opposite is a defect regardless of which
    /// fact happens to be true.
    ///
    /// "rotating" rather than "enabled" on purpose. The button's label is the
    /// verb (`tcr enable` / `tcr disable`, and it should stay a verb), so an
    /// "ENABLED" pill next to a "Disable" button puts two words with the same
    /// stem beside each other and asks the reader to notice which is a state.
    /// Rotation is the vocabulary nothing else in the row uses, and it names the
    /// consequence the operator actually cares about: whether requests land here.
    ///
    /// The in-rotation case is drawn in `Tok.inkFaint` rather than a status hue:
    /// it is the normal state of twelve of thirteen rows, and colouring the
    /// unremarkable case would spend the panel's colour budget on it. Colour
    /// stays with quota, which is the thing worth scanning for.
    ///
    /// This pill answers ONE question — can this account be picked for
    /// traffic — and there are at least THREE independent ways for the
    /// answer to be no, and this build can now see all three:
    /// `disabled` (an operator's own choice), `account.health ==
    /// .needsRelogin` (`src/manager/select.rs:931` hard-excludes an
    /// `AccountStatus::Error` account from selection exactly like a disabled
    /// one, even though `disabled` itself reads false), and the third —
    /// `select.rs:809-822` also excludes an account whose `quota.status ==
    /// Some("rejected")`, Anthropic's own verdict — while the snapshot
    /// `status` this app decodes stays `"active"` (`snapshot.rs:142-153` only
    /// ever rewrites `Throttled`).
    ///
    /// That third gate is read from the wire's `gate` field
    /// (``Account/isRejected``), which the server has emitted since the status
    /// wire crate landed. An earlier version of this comment said `tcr status
    /// --json` emitted no gate field at all and deferred the fix; the field was
    /// already there. It is decoded as an OPTIONAL, so an older server (absent
    /// field) degrades to the previous behaviour rather than to a false claim
    /// in either direction.
    ///
    /// A row broken by the SECOND gate draws NEITHER "rotating" nor "parked":
    /// "parked" claims an operator decision that never happened, and
    /// "rotating" claims traffic can land here, which `select.rs` refuses.
    /// Silence is the case this pill was rewritten to avoid, but the reason
    /// silence used to be dangerous was that nothing nearby said the row
    /// could not serve — here the red NEEDS RE-LOGIN pill already says
    /// exactly that, unambiguously, so a second pill would only have two ways
    /// left to be wrong.
    /// Row-level marker for the identity-bound control account, drawn beside
    /// the name so it survives without opening the gear menu — the gear's own
    /// checkmark state is one click away, and `ImageRenderer` cannot rasterise
    /// a `Menu` at all (see the harness note on `--render-states`), so this is
    /// also the only place this fact is ever visually verifiable outside a live
    /// run.
    ///
    /// `Tok.accent` — the system control-accent colour, not one of the
    /// quota/rotation status hues (`ok`/`near`/`spent`/`unmeasured`/`disabled`/
    /// `unknown`) and not `Tok.awake` (already spoken for by keep-awake mode,
    /// a genuinely different fact). "Control account" is a designation, not a
    /// measured state, and `accent` is the one token in this palette already
    /// reserved for exactly that distinction elsewhere in the app.
    ///
    /// Silent — not merely dim — when `control.unavailable` or nothing is set:
    /// a phantom checkmark on an older `tcr` build (which cannot even confirm
    /// the concept exists) would be worse than the feature simply not
    /// appearing yet.
    /// The account's plan, beside the name — `MAX 20X`, `TEAM 5X`,
    /// `TEAM STANDARD`.
    ///
    /// It sits next to the NAME rather than out with the rotation pills because
    /// it is a fact about which account this is, not about how it is behaving
    /// right now. On this fleet the same email appears twice, and before this
    /// tag existed the two rows were visually identical — the panel simply had
    /// no way to say which was the personal Max org and which the company Team.
    ///
    /// Silent when the server reports no plan (never profiled, or a build that
    /// predates the field). A guessed label here would defeat the one thing the
    /// tag is for. `Tok.inkFaint` — the neutral hue, not one of the quota or
    /// rotation status colours: a plan is a designation, not a measured state,
    /// and tinting it like one would make an ordinary account look alarming.
    ///
    /// Dim TEXT rather than a ``StatusPill``, and that is a width decision, not
    /// a taste one. As a pill this read `TEAM STANDARD` — uppercased, tracked,
    /// padded and bordered — and the row is `Tok.panelWidth` (380pt) wide: the
    /// pill's chrome pushed the name past its budget and rendered it as `h…m`.
    /// A tag whose whole job is telling two same-named rows apart must not be
    /// the thing that erases the name. Plain text at the same font, with no
    /// chrome and no uppercasing, is narrow enough for the worst row (unmeasured
    /// + rotating + the longest label) and still reads as secondary.
    /// The row's DESIGNATIONS — what this account IS — on their own line under
    /// the name: its plan, and the groups it belongs to.
    ///
    /// This line exists because the row overflowed the panel. Every pill is
    /// `fixedSize()`, so a single `HStack` carrying `CONTROL` + `GROUP ONLY` +
    /// a quota pill + two group tags + the plan has a MINIMUM width past
    /// `Tok.panelWidth` (380pt) before the name gets a single point — and the
    /// enclosing `.frame(width:)` centres content it cannot fit rather than
    /// containing it, so both card edges were clipped and the header truncated.
    /// No amount of layout priority on the name fixes that: the name can shrink
    /// to nothing and the pills alone still overflow.
    ///
    /// The split is by MEANING, not just to save width, which is why it reads
    /// as a design rather than a workaround: the first line is identity and
    /// LIVE STATE (who this is, whether traffic lands here right now), and this
    /// one is standing facts that change on a plan upgrade or an operator's
    /// group edit. The group tags moved here with the plan because two long
    /// group names are the single biggest contributor to the width — the live
    /// fleet's `HENRY-TEAM-PARKED` alone is wider than three status pills.
    ///
    /// Drawn only when there is something to say; an account with no plan, no
    /// groups and a bare-email name reserves no space at all.
    ///
    /// The org half of the name leads this line — before the plan — because it
    /// is part of the row's IDENTITY rather than a fact about it. It is here at
    /// all only because line one has no room for it; putting it first keeps the
    /// two halves as close to each other as two lines allow.
    ///
    /// ``Account/groupTags`` with the ONE tag naming the section this row is
    /// already drawn under removed. An account in two groups renders once per
    /// group, so inside the `henry-team-parked` heading a `HENRY-TEAM-PARKED`
    /// pill restated the heading directly above it on every card in that
    /// section — no reader needed the reminder, and it was the single biggest
    /// remaining contributor to this line's width. The other groups an account
    /// belongs to still show: this drops exactly the one tag whose fact is
    /// already on screen, never the others.
    private var visibleGroupTags: [GroupTag] {
        guard let enclosingGroupName else { return account.groupTags }
        return account.groupTags.filter { $0.name != enclosingGroupName }
    }

    @ViewBuilder
    private var designationsLine: some View {
        if account.ref.displayHalves.orgTag != nil || account.plan != nil
            || account.windowUsageLabel != nil || !visibleGroupTags.isEmpty
        {
            HStack(spacing: Tok.tightSpacing) {
                orgIndicator
                planIndicator
                // At most two tags, the rest collapsed into a `+N` chip whose
                // tooltip names them all. The cap stays even with a line to
                // itself: an account can be in many groups, and this line has a
                // budget too — it is just a far larger one.
                ForEach(Array(visibleGroupTags.prefix(2))) { tag in
                    GroupChip(tag: tag)
                }
                if visibleGroupTags.count > 2 {
                    StatusPill("+\(visibleGroupTags.count - 2)", tint: Tok.inkFaint)
                        .help(visibleGroupTags.map(\.name).joined(separator: ", "))
                }
                Spacer(minLength: 0)
            }
        }
    }

    /// The `/<org-slug>` half of the name, verbatim and never truncated —
    /// `fixedSize()`, like every other tag on this line.
    ///
    /// Rendered dim, at the tag font, because it is a continuation of the name
    /// above rather than a separate label: it reads as part of an identity that
    /// wrapped, which is what it is. The text is EXACTLY what the operator would
    /// type, separator included, so a name can be reassembled by reading the two
    /// lines left to right.
    ///
    /// Absent on a bare-email name, which is every row on a fleet where nobody
    /// holds two orgs — those rows look exactly as they always have.
    @ViewBuilder
    private var orgIndicator: some View {
        if let orgTag = account.ref.displayHalves.orgTag {
            Text(orgTag)
                .font(Tok.pillFont).lineSpacing(Tok.detailLineSpacing)
                .foregroundStyle(Tok.inkFaint)
                .fixedSize()
                .textSelection(.enabled)
                .help(
                    "The organization half of this account's name — the full name is "
                        + "\(account.name), which is what every `tcr` command takes."
                )
        }
    }

    @ViewBuilder
    private var planIndicator: some View {
        if let plan = account.plan, !plan.isEmpty {
            Text(planLineText(plan: plan))
                .font(Tok.pillFont).lineSpacing(Tok.detailLineSpacing)
                .foregroundStyle(Tok.inkFaint)
                .fixedSize()
                .help("This account's plan, as Anthropic reports it for its organization.")
        } else if let usageLabel = account.windowUsageLabel {
            // No plan on the wire, but there is still a spend figure to show
            // — draw it alone rather than losing it, the same "an absent
            // measurement never renders as a fabricated value" rule this
            // slot already followed when it lived on the quota row.
            Text(usageLabel)
                .font(Tok.pillFont).lineSpacing(Tok.detailLineSpacing)
                .foregroundStyle(usageFigureStyle)
                .fixedSize()
        }
    }

    /// "Max 20x · $540 · 1.5M output tokens this week" —
    /// `docs/design/panel-tabs-mockup.html`'s plan line, F8: spend and
    /// output tokens moved off the quota grid and onto this line, which
    /// already carried the plan name. `account.windowUsageLabel` is
    /// `"$4.20 · 48k out"` (see its own doc-comment for the day-fallback and
    /// partial-pricing markers it already carries); `nil` when the proxy
    /// never measured this account, and the plan then draws alone rather
    /// than growing a placeholder.
    private func planLineText(plan: String) -> String {
        guard let usageLabel = account.windowUsageLabel else { return plan }
        return "\(plan) · \(usageLabel)"
    }

    @ViewBuilder
    private var controlIndicator: some View {
        if control.isControl(account.name) {
            StatusPill("control", tint: Tok.accent)
                .help("This account is held out of rotation as the control account.")
                // Fades in and out rather than snapping. Safe against the
                // height hazard: this line's quota-state pill (`rotationPill`'s
                // neighbour below) is unconditional and the same height as
                // every `StatusPill`, so this line's height never changes
                // whether this pill is drawn or not.
                .transition(.opacity)
        }
    }

    /// Every branch below that draws a `StatusPill` carries `.transition(.opacity)`:
    /// this line's height is pinned regardless, by the quota-state pill further
    /// along the same `HStack` (`information`, `FleetView.swift`), which is
    /// never `EmptyView` — so fading this one in and out changes no measured
    /// height even on the two branches that go empty.
    @ViewBuilder
    private var rotationPill: some View {
        if suppressParkedPill && (account.disabled || account.isParkedByGroup) {
            EmptyView()
        } else if account.disabled {
            StatusPill("parked", tint: Tok.disabled)
                .help("Out of the rotation — `tcr` sends this account no traffic.")
                .transition(.opacity)
        } else if account.isParkedByGroup {
            // The SAME word as the row's own `disabled` above, deliberately:
            // the consequence is identical (no traffic lands here), and giving
            // one fact two names is how a reader learns to distrust both. What
            // differs is the cause, so the help names the group to unpark —
            // otherwise an operator sees a parked row with an enabled account
            // and has nowhere to go.
            //
            // Ordered after `disabled` for the same reason the server orders
            // its gate that way: a row benched by hand says so, and is never
            // reported as parked-by-group.
            StatusPill("parked", tint: Tok.disabled)
                .help(
                    "Out of the rotation — every member of "
                        + "\(account.parkedGroupNames.joined(separator: ", ")) is held back. "
                        + "`tcr group unpark "
                        + "\(account.parkedGroupNames.first ?? "")` puts them back, live."
                )
                .transition(.opacity)
        } else if account.health == .needsRelogin {
            EmptyView()
        } else if account.isRejected {
            // Anthropic's own verdict, and the third exclusion named above.
            // Drawn INSTEAD of "rotating": the router will never select this
            // account, so claiming it is in rotation is the exact
            // "misread as its own opposite" defect the other two gates were
            // fixed for. Nothing else on the row says it — `disabled` is false,
            // `status` reads "active", and the quota bars can look healthy.
            EmptyView()
        } else if account.servesGroupTrafficOnly {
            // Predicate lives on the model (`Account.servesGroupTrafficOnly`) so
            // it is testable without SwiftUI and matches the server's ANY rule.
            //
            // This branch exists because its absence was itself the bug: a
            // reserved account rendered `GIL` + `ROTATING` side by side, which
            // reads as "tagged AND in the pool" — the exact state an operator
            // reserves a group to prevent, shown as if nothing had happened.
            StatusPill("group only", tint: Tok.inkFaint)
                .help(
                    "Reserved — serves only requests that ask for one of its "
                        + "groups by name. Pool traffic never lands here, and this "
                        + "group's own traffic never leaves it: a `--group` request "
                        + "with no member free waits, or fails, rather than using "
                        + "another account."
                )
                .transition(.opacity)
        } else {
            StatusPill("rotating", tint: Tok.inkFaint)
                .help(
                    "In the rotation — this account can be picked for traffic, as "
                        + "far as this build can tell. A quota rejection from "
                        + "Anthropic can also exclude an account without changing "
                        + "its status; TcrBar cannot see that gate yet."
                )
                .transition(.opacity)
        }
    }

    /// One utterance for the whole row.
    ///
    /// Without this, VoiceOver walks roughly eight separate elements per account
    /// — name, pills, bar, two percentage runs, status, countdown — which is
    /// about a hundred stops to traverse thirteen accounts. The toggle stays
    /// OUTSIDE the combined element so it remains separately focusable and
    /// actionable.
    private var rowAccessibilityLabel: String {
        var parts = [account.name]
        // Spoken right after the name, for the same reason the tag is drawn
        // there: on a fleet holding one email twice, the plan is what tells a
        // listener WHICH of two identically-named rows they are on. Without it
        // a VoiceOver user hears the same name announced twice with no way to
        // tell the personal Max org from the company Team one.
        if let plan = account.plan, !plan.isEmpty {
            parts.append(plan)
        }
        // Spoken beside the name for the same reason `controlIndicator` is
        // drawn beside it: this is an identity fact, not a rotation/quota one,
        // and a VoiceOver user reaching the row should not have to open the
        // gear menu to learn it.
        if control.isControl(account.name) {
            parts.append("control account")
        }
        // Spoken in both directions, for the same reason the pill is drawn in
        // both: silence is not a state, and a VoiceOver user has even less to
        // infer it from than a sighted one. A broken row says neither
        // "rotating" nor "parked" — same reason `rotationPill` draws neither:
        // `disabled` reads false, so "parked" would claim an operator
        // decision that never happened, and "rotating" would claim traffic
        // can land here, which `src/manager/select.rs:931` refuses exactly
        // like a disabled account. The detail below already names the real
        // cause, so this element is not left silent either.
        if account.disabled {
            parts.append("parked, out of rotation")
        } else if account.isParkedByGroup {
            // Names the group, because a listener has no help tooltip to reach
            // for — the pill's own text is the same word either way.
            parts.append(
                "parked with group \(account.parkedGroupNames.joined(separator: ", ")), "
                    + "out of rotation")
        } else if account.health == .needsRelogin {
            parts.append("not in rotation")
        } else if account.isRejected {
            parts.append("rejected by Anthropic, not in rotation")
        } else {
            parts.append("rotating")
        }
        // Mirrors the pill's three cases. A VoiceOver user hearing "never
        // probed" about an account whose probe errored is told the same wrong
        // cause a sighted user was, with less to correct it from.
        // Broken beats the probe-based cases: a rejected refresh token is a
        // known cause with a known remedy, and speaking "never probed" over it
        // is the same wrong cause a sighted user was told, with less to correct
        // it from.
        if account.health == .needsRelogin {
            // The spoken half of the same fix as `quotaTint`: dropping the
            // number here (as an earlier round of this did) under-informs a
            // VoiceOver user exactly where a sighted one still sees a muted
            // bar and grey digits — deleting real information rather than
            // demoting it. `hasQuotaEvidence` is false on the never-probed
            // shape (the `04b` scene), where there truly is no number to
            // qualify, so only the probed-then-broken shape (`04c`) gets the
            // longer phrase.
            if account.hasQuotaEvidence {
                parts.append(
                    "refresh token rejected, re-login to restore traffic. Last "
                        + "reading before rejection: \(account.quotaState.token), "
                        + "\(QuotaFormat.percent(account.quota)) used — unreachable "
                        + "until re-login"
                )
            } else {
                parts.append("refresh token rejected, re-login to restore traffic")
            }
        } else if account.hasQuotaEvidence {
            parts.append("\(account.quotaState.token), \(QuotaFormat.percent(account.quota)) used")
        } else if account.probeStatus.isFailure {
            parts.append("quota probe \(account.probeStatus.token), quota unknown")
        } else {
            parts.append("never probed, quota unknown")
        }
        if let hold = account.soonestHold { parts.append(hold.countdownLabel) }
        // The spoken half of the 5h line's right-hand figures. Silent when the
        // account was not measured, exactly as the printed slot is — speaking
        // "zero dollars" over an absent measurement is the one thing this
        // whole feature must not do.
        if let usageLabel = account.windowUsageSpokenLabel { parts.append(usageLabel) }
        // The spoken half of the 7d line's Fable slot. Silent when the server
        // did not measure that window, exactly as the printed slot is — and
        // spoken when it did, because a gate that decides whether Fable
        // requests can land here is not a sighted-user-only fact.
        if let fable = account.fableWeeklySpokenLabel(now: Date()) { parts.append(fable) }
        if let failure = accounts.failure(for: account.ref) {
            parts.append("last action failed: \(failure.summary)")
        }
        if let failure = removeController.failure(for: account.ref) {
            parts.append("last action failed: \(failure.summary)")
        }
        if let refused = groupController.failure(forAccount: account.ref) {
            parts.append("group '\(refused.group)' failed: \(refused.failure.summary)")
        }
        // Spoken for the same reason the pill is: a confirmation only a sighted
        // user gets is half built, and the `✓` in `rowLabel` is punctuation to a
        // screen reader.
        if let verdict = accounts.verdict(for: account.ref, reportedDisabled: account.disabled) {
            parts.append(verdict.spokenLabel)
        }
        // Same reason `removalNoticeLine` is drawn at all: a VoiceOver user
        // gets no other signal that the delete landed but the row did not
        // change, since nothing here re-derives the fleet from the config.
        if removeController.needsRestart(account.ref) {
            parts.append("removed from config and stopped, stays listed as disabled until the proxy restarts")
        }
        return parts.joined(separator: ", ")
    }

    var body: some View {
        rowContent
            // The card box — radius, padding, fill and border — is
            // ``SwiftUI/View/panelCard()``, shared with the Sessions and Tools
            // tabs so all three read as one design. It carries the concentric
            // inset this comment used to describe: an outer radius with no
            // matching padding draws a border that bites into its own content
            // at the corners, which is why the padding and the radius are one
            // definition and not two call sites.
            //
            // That inset is also the row's ONLY inner padding now. `rowContent`
            // carried a second one (`Tok.rowPaddingV`) from before this card had
            // a border, when a row needed its own breathing room; inside a
            // bordered card it was 4pt of nothing.
            .panelCard()
            .contextMenu { contextMenuItems }
    }

    /// The row's own layout, unchanged from before the border was added except
    /// for its name: this used to be `body` directly. Split out so the border
    /// and the context menu are the ONE place that wraps every row shape,
    /// rather than three copies of the same background/overlay/contextMenu
    /// pair — the broken-row branch and the normal branch would otherwise have
    /// to repeat it identically.
    @ViewBuilder
    private var rowContent: some View {
        // A broken row draws its two buttons on their OWN line, below the name
        // and pills, rather than beside `information` the way `toggleButton`
        // alone sits for every other row. Measured, not assumed: beside
        // `information`, `ROTATING` + `NEEDS RE-LOGIN` + `Re-login…` +
        // `Disable` do not fit in the row's 356pt (`fleetActions`'s own
        // truncation bug, documented above, is exactly this failure mode) —
        // the name collapsed to a single truncated character. Every other row
        // keeps the original layout unchanged, because it already fits.
        if account.health == .needsRelogin {
            VStack(alignment: .leading, spacing: Tok.tightSpacing) {
                information
                    .accessibilityElement(children: .combine)
                    .accessibilityLabel(rowAccessibilityLabel)
                HStack(spacing: Tok.tightSpacing) {
                    Spacer()
                    reloginButton
                    toggleButton
                }
            }
        } else {
            HStack(alignment: .top, spacing: Tok.tightSpacing) {
                information
                    .accessibilityElement(children: .combine)
                    .accessibilityLabel(rowAccessibilityLabel)
                accountActionsMenu
            }
        }
    }

    /// Actions beyond the single toggle, reached three ways: the gear menu
    /// beside the row, right-click (or Control-click / two-finger click) for
    /// a context menu, and — on a broken row only — the standalone
    /// ``reloginButton``. Built from what ``AccountController`` and the
    /// row's own `onRelogin` closure already expose — nothing here calls
    /// anything new. All three routes share this one `@ViewBuilder` so the
    /// gear and the context menu cannot drift into two different notions of
    /// what the row can do.
    ///
    /// Deliberately does NOT add routing or pin controls: that is a separate
    /// feature, not this row's to invent.
    @ViewBuilder
    private var contextMenuItems: some View {
        let enabling = account.disabled
        Button(enabling ? "Enable" : "Disable") {
            Task { await performToggle(enabling: enabling) }
        }
        .disabled(accounts.isPending(account.ref))
        Button("Re-login…") { onRelogin() }
            .help(
                "Opens `tcr login --account` in a Terminal window, requesting "
                    + "this exact account. `tcr` refuses to save if the browser "
                    + "hands back a different one. The login it starts now mints "
                    + "a credential good for a year, so running it on a healthy "
                    + "row is a safe way to get ahead of the old one expiring."
            )
        Divider()
        // Hidden entirely while `control` cannot answer the question at all
        // (``ControlAccountController/unavailable``) — an older `tcr` has no
        // `control` subcommand, and an action this build cannot even read back
        // is worse than no action: a click would exit non-zero with a message
        // about a route that does not exist, on every single row, forever.
        if !control.unavailable {
            if control.isControl(account.name) {
                Button("Clear Control Account") {
                    Task { await performSetControl(name: nil, key: account.id) }
                }
                .disabled(control.isPending(account.id))
            } else {
                Button("Use as Control Account") {
                    // `name` is what `tcr control` stores (a bare string — see
                    // `ControlAccountController.isControl`), while `key` is
                    // this row's own identity, so the spinner lands on the row
                    // that was clicked even when two rows share a name.
                    Task { await performSetControl(name: account.name, key: account.id) }
                }
                .disabled(control.isPending(account.id))
            }
        }
        Divider()
        Button("Copy Account Name") {
            copyToPasteboard(account.name)
        }
        // The token is a secret: it goes from `tcr token`'s stdout straight
        // to the pasteboard and is never rendered, logged or kept.
        Button("Copy Access Token") {
            Task { await performCopyToken() }
        }
        // Distinct wording from the button above on purpose: "Copy" hands over
        // a string that already exists and dies in hours, "Mint" makes a new
        // one that does not — the confusion between the two is exactly what
        // prompted this item to exist.
        Button("Mint Long-Lived Token…") {
            onMint()
        }
        Divider()
        groupMenuItems
        Divider()
        // Last, and alone below its own rule — same placement logic as
        // `dangerZone`'s "Take over port…": the most expensive, least
        // reversible action on the row does not get to sit beside routine
        // ones a misclick could land on instead.
        Button("Delete Account…") {
            confirmDeleteAccount()
        }
        .disabled(removeController.isPending(account.ref))
    }

    /// One entry per ``Account/groupMenuActions``, wired directly to
    /// ``GroupController``'s `remove(account:from:)`/`add(account:to:)` — the
    /// only way to change group membership from this panel now that the
    /// section-header menus are gone. Kept as a top-level `@ViewBuilder` (not
    /// folded into `contextMenuItems`'s body) so ``Account/groupMenuActions``'s
    /// own doc-comment, not this one, is the single place the menu's SHAPE is
    /// explained.
    @ViewBuilder
    private var groupMenuItems: some View {
        ForEach(account.groupMenuActions, id: \.self) { action in
            switch action {
            case .remove(let group):
                // First, and disabled because there is nothing to click — this
                // states a fact about the group, it does not offer to change it.
                // Without it the panel renders a group that serves nothing
                // exactly like one that works: a member, a colour, a healthy
                // row. That is how a group whose only member was the control
                // account survived on the live fleet, silently falling back on
                // every request.
                if !GroupRouting.routes(
                    group: group, accounts: allAccounts, controlName: control.current)
                {
                    Button("⚠︎ “\(group)” routes nothing — control account only") {}
                        .disabled(true)
                } else if GroupRouting.allowsControlAccount(group: group, accounts: allAccounts) {
                    // The opted-in state gets its OWN line rather than just the
                    // absence of the warning above. Silence would be cheaper and
                    // is the wrong call here: "opted in" and "I forgot to opt in"
                    // would look identical, and an unroutable group that looked
                    // fine is the exact failure this whole menu section exists
                    // to end.
                    Button("“\(group)” may use the control account") {}
                        .disabled(true)
                }
                // The USE command, above the two removals. This slot used to copy
                // `tcr group rm <group> <account>` — the twin of the button
                // right below it — which made three consecutive entries all
                // spell a deletion and offered no way at all to start a session
                // on the group. Copying the command that administers a label was
                // never what anyone wanted off a row that already carries it.
                let copyRun = GroupCommand.CopyCommandMenuEntry(
                    arguments: GroupCommand.runArguments(group: group))
                Button(copyRun.title) {
                    copyToPasteboard(copyRun.copiedText)
                }
                // Same distinction as the row-level item: this mints a NEW
                // long-lived token for every member of the group, it does not
                // copy anything that already exists.
                Button("Mint Long-Lived Tokens for “\(group)”…") {
                    onMintGroup(group)
                }
                // One click, either direction, with the current state on the
                // item itself — the only administrative entry in this menu that
                // takes effect without a restart, which is why it can be a
                // toggle at all rather than an instruction to go and edit the
                // config. It sits above the two removals because holding a
                // group back is the reversible thing to want; deleting it is
                // not.
                parkToggle(for: group)
                Button("Remove from \(group)") {
                    Task { await groupController.remove(account: account.ref, from: group) }
                }
                Button("Delete group “\(group)” for everyone…") {
                    confirmDeleteGroup(group)
                }
            case .removeAll:
                Button("Remove from all groups") {
                    Task {
                        for group in (account.groups ?? []) {
                            await groupController.remove(account: account.ref, from: group)
                        }
                    }
                }
            case .addToGroup:
                addToGroupMenu
            }
        }
    }

    /// The Park/Unpark toggle for one group, with a checkmark showing the
    /// state it is in right now.
    ///
    /// The state is read from the FLEET (`allAccounts`), not from this row: a
    /// group is parked or not for everyone, and reading this row alone would
    /// draw an unchecked item on a member the wire happened to omit. Disabled
    /// while the call is in flight, so a second click cannot queue a second
    /// subprocess against the same label.
    @ViewBuilder
    private func parkToggle(for group: String) -> some View {
        let parked = allAccounts.contains { ($0.parkedGroups ?? []).contains(group) }
        Button {
            Task { await groupController.setParked(group: group, parked: !parked) }
        } label: {
            // A leading checkmark rather than `Toggle`: `Menu` contents are
            // rasterised nowhere (see `RenderStates`' header) and a plain
            // Button with an explicit mark is what the rest of this menu uses.
            Label(
                parked ? "Unpark group “\(group)”" : "Park group “\(group)”",
                systemImage: parked ? "checkmark" : "pause.circle")
        }
        .disabled(groupController.isPending("park/\(group)"))
    }

    /// Every group this account is not already a member of, fleet-wide —
    /// the population ``groupMenuItems``'s "Add to group…" submenu offers.
    /// Disabled rather than hidden when empty (no group exists yet anywhere
    /// in the fleet): a missing submenu reads as "this row cannot be added
    /// to a group", which is not the true reason.
    private var candidateGroupsToAdd: [String] {
        let existing = Set(account.groups ?? [])
        let everyGroup = Set(allAccounts.flatMap { $0.groups ?? [] })
        return everyGroup.subtracting(existing).sorted()
    }

    /// Never disabled, unlike before this round: "New group…" is always a
    /// usable entry even for a fleet with no groups at all yet, which is the
    /// exact gap this menu exists to close.
    @ViewBuilder
    private var addToGroupMenu: some View {
        Menu("Add to group…") {
            Button("New group…") { presentNewGroupPrompt() }
            if !candidateGroupsToAdd.isEmpty {
                Divider()
                ForEach(candidateGroupsToAdd, id: \.self) { group in
                    Button(group) {
                        Task { await groupController.add(account: account.ref, to: group) }
                    }
                    let copyAdd = GroupCommand.CopyCommandMenuEntry(
                        arguments: GroupCommand.addArguments(group: group, account: account.name))
                    Button(copyAdd.title) {
                        copyToPasteboard(copyAdd.copiedText)
                    }
                }
            }
        }
    }

    /// Every group anywhere in the fleet, not just this account's own —
    /// what ``NewGroupName/evaluate(_:existingGroups:)`` checks a typed name
    /// against so creating a group can never silently become a plain add
    /// into one that already exists.
    private var everyGroupFleetWide: Set<String> {
        Set(allAccounts.flatMap { $0.groups ?? [] })
    }

    /// Prompts for a new group's name with the same accessory-text-field
    /// `NSAlert` shape as ``confirmTakeover()`` elsewhere in this file —
    /// there is no other text-entry affordance in this panel to reuse.
    /// Re-prompts on an invalid or duplicate name so a typo does not have to
    /// be re-triggered from the context menu; Escape or Cancel back out with
    /// no call made.
    private func presentNewGroupPrompt() {
        let alert = NSAlert()
        alert.messageText = "New group"
        alert.informativeText = "This account is added to the group as soon as it's created."
        let field = NSTextField(frame: NSRect(x: 0, y: 0, width: 220, height: 24))
        field.placeholderString = "Group name"
        alert.accessoryView = field
        alert.window.initialFirstResponder = field
        let create = alert.addButton(withTitle: "Create")
        alert.addButton(withTitle: "Cancel")
        alert.buttons.last?.keyEquivalent = "\u{1b}"
        create.keyEquivalent = "\r"

        guard alert.runModal() == .alertFirstButtonReturn else { return }
        let typed = field.stringValue
        let outcome = NewGroupName.evaluate(typed, existingGroups: everyGroupFleetWide)
        switch outcome {
        case .valid(let name):
            Task { await groupController.add(account: account.ref, to: name) }
        case .rejected, .duplicate:
            let failureAlert = NSAlert()
            failureAlert.alertStyle = .warning
            failureAlert.messageText = "Can't create that group"
            failureAlert.informativeText = outcome.rejectionMessage ?? "That name isn't usable."
            failureAlert.addButton(withTitle: "OK")
            failureAlert.runModal()
        }
    }

    /// Confirms before ``GroupController/removeAll(group:)`` — same
    /// destructive-`NSAlert` shape as ``confirmTakeover()`` (critical style,
    /// destructive button unlabeled-as-default, Cancel as the actual
    /// default). This is the one group action that reaches past this row:
    /// it removes `group` from every account on the fleet, not just
    /// ``account``, so the alert text says that explicitly rather than
    /// reading like an ordinary "remove from this group."
    private func confirmDeleteGroup(_ group: String) {
        let alert = NSAlert()
        alert.alertStyle = .critical
        alert.messageText = "Delete group “\(group)” for every account?"
        alert.informativeText = """
            This removes “\(group)” from every account on the fleet, not just \
            \(account.name) — every row currently tagged with it loses the tag.
            """
        let delete = alert.addButton(withTitle: "Delete Group")
        delete.hasDestructiveAction = true
        alert.addButton(withTitle: "Cancel")
        // Cancel is the default: Return and Escape both back out.
        alert.window.defaultButtonCell = nil
        alert.buttons.last?.keyEquivalent = "\r"
        delete.keyEquivalent = ""

        guard alert.runModal() == .alertFirstButtonReturn else { return }
        Task { await groupController.removeAll(group: group) }
    }

    /// Confirms before ``RemoveAccountController/remove(account:org:)`` — same
    /// destructive-`NSAlert` shape as ``confirmTakeover()``/``confirmDeleteGroup(_:)``
    /// (critical style, destructive button unlabeled-as-default, Cancel as the
    /// actual default). Names the account, states the consequence in plain
    /// language (this is not reversible from the UI — the saved OAuth tokens
    /// are gone and getting the account back needs a fresh `tcr login`), and
    /// states up front that the row stays listed as disabled until the proxy
    /// restarts, even though the account itself stops serving right away.
    private func confirmDeleteAccount() {
        let alert = NSAlert()
        alert.alertStyle = .critical
        alert.messageText = "Delete account “\(account.name)”?"
        alert.informativeText = """
            This removes “\(account.name)” and its saved credentials from the config. \
            It is not reversible from here — getting it back means running `tcr login` \
            again from scratch.

            The account stops serving immediately, but this row stays listed here as \
            disabled until the proxy restarts.
            """
        let delete = alert.addButton(withTitle: "Delete Account")
        delete.hasDestructiveAction = true
        alert.addButton(withTitle: "Cancel")
        // Cancel is the default: Return and Escape both back out.
        alert.window.defaultButtonCell = nil
        alert.buttons.last?.keyEquivalent = "\r"
        delete.keyEquivalent = ""

        guard alert.runModal() == .alertFirstButtonReturn else { return }
        Task { await removeController.remove(account: account.ref) }
    }

    /// Shared by "Copy Account Name" and the per-group command copy — one
    /// place that clears then sets, so every copy in this menu behaves the
    /// same way.
    /// `tcr token <name>` off the main actor, then the pasteboard. A failure
    /// replaces any earlier one on this row; a success clears it.
    ///
    /// The name is enough because `tcr` guarantees it is unique. On a fleet
    /// holding one email twice it was not, and this refused with `'…' is
    /// ambiguous — matches 2 accounts`, so neither row's token could be copied.
    private func performCopyToken() async {
        switch await TokenCommand.fetch(query: account.name) {
        case .success(let token):
            copyToPasteboard(token)
            tokenCopyFailure = nil
        case .failure(let failure):
            tokenCopyFailure = failure
        }
    }

    private func copyToPasteboard(_ text: String) {
        let pasteboard = NSPasteboard.general
        pasteboard.clearContents()
        pasteboard.setString(text, forType: .string)
    }

    /// The one place `tcr control` is actually run for this row — set when
    /// picked from another row's menu, or cleared when picked from the row that
    /// currently holds it. Mirrors ``performToggle(enabling:)``: exit 0 is
    /// permission to re-read, never the outcome itself, so `control.refresh()`
    /// runs afterward and the row draws whatever it actually says.
    private func performSetControl(name: String?, key: String) async {
        let attempt = await control.setControl(name: name, key: key)
        guard case .accepted = attempt else { return }
        await control.refresh()
    }

    /// The visible affordance for `contextMenuItems`. Right-click was the
    /// only way to reach Enable/Disable, Re-login… and Copy Account Name
    /// before this — undiscoverable, since nothing on the row hinted a menu
    /// existed. Replaces the row's old trailing `toggleButton` text button
    /// (still used, unchanged, on the `.needsRelogin` layout — see the
    /// doc-comment there) so the account name also gets back the width that
    /// button's text ("Disable"/"Enabling…") used to take, which matters
    /// because the name already truncates.
    ///
    /// `.menuStyle(.borderlessButton)` rather than the `.bordered` style
    /// every other control in this panel uses: those are all text buttons,
    /// and a bordered icon-only control reads as heavier chrome than a
    /// 13pt row wants. This is the one icon-only control in the panel, so
    /// there is no existing icon-menu style to match instead.
    @ViewBuilder
    private var accountActionsMenu: some View {
        if snapshotMode {
            // `ImageRenderer` cannot draw a `Menu` at all — it rasterises a
            // yellow prohibition glyph in its place, which is what shipped
            // in the README hero before this branch existed. A snapshot
            // draws the label on its own, non-interactive, so the PNG shows
            // the gearshape a user actually sees on the live panel.
            accountActionsMenuLabel
                .controlSize(.small)
                .fixedSize()
                .accessibilityLabel("Account actions for \(account.name)")
                .help("Enable/disable, re-login, or copy the account name.")
        } else {
            Menu {
                contextMenuItems
            } label: {
                accountActionsMenuLabel
            }
            .menuStyle(.borderlessButton)
            .controlSize(.small)
            .fixedSize()
            .accessibilityLabel("Account actions for \(account.name)")
            .help("Enable/disable, re-login, or copy the account name.")
        }
    }

    /// The gearshape glyph shared by the live `Menu` and its `snapshotMode`
    /// stand-in, so the two can never draw two different icons.
    private var accountActionsMenuLabel: some View {
        Image(systemName: "gearshape")
            .font(Tok.bodyFont).lineSpacing(Tok.bodyLineSpacing)
            .foregroundStyle(Tok.inkDim)
            // The Menu's own `.accessibilityLabel` below names the
            // control; without hiding the glyph too, VoiceOver reads
            // both the image ("gearshape, image") and the label,
            // announcing the same control twice.
            .accessibilityHidden(true)
    }

    /// The one place `tcr enable`/`tcr disable` is actually run. Shared by
    /// ``toggleButton`` and the context menu's own entry so the two can never
    /// drift into two different notions of what a toggle does — see the
    /// doc-comment on `toggleButton`'s old inline `Task` for why the
    /// read-back-and-record dance below is not optional.
    private func performToggle(enabling: Bool) async {
        // Exit 0 is not the outcome — it is permission to go and find out
        // what the outcome was. The refresh's own result is compared
        // against what was asked, and the row says which thing happened.
        //
        // `notice` is threaded through rather than dropped: `tcr` reports
        // half-done work (a park the config cannot persist, a proxy too old
        // for the route) on stderr while exiting 0, and the read-back
        // cannot see it — `disabled` flipped, so the comparison confirms.
        // Losing it here is what let the row stamp `parked ✓` on a change
        // that would not survive a restart.
        let attempt = await accounts.setEnabled(enabling, account: account.ref)
        guard case .accepted(let notice) = attempt else { return }
        let readback = await onChanged()
        accounts.record(
            readback: readback,
            requestedEnabled: enabling,
            account: account.ref,
            notice: notice
        )
    }

    /// ``AccountRef/displayHalves``'s email half, with a break OPPORTUNITY
    /// inserted right after `@` — never a break forced.
    ///
    /// An email has no spaces, so SwiftUI's line breaker sees it as one
    /// unbreakable word and, when it overflows the row, falls back to
    /// splitting wherever it runs out of width — `01g-widest-row`'s
    /// `henry.fitzgerald@example.com` wrapped as `henry.fitzgerald@ex` /
    /// `ample.com`, a character split mid-domain that defeats scanning. A
    /// zero-width space (`U+200B`) renders as nothing but IS a legal break
    /// point, so a wrap now lands where a reader already expects an address
    /// to fold: right after the `@`.
    ///
    /// This string is what the `Text` below DRAWS, not what a user should
    /// COPY: the inserted `U+200B` is invisible but real, so a drag-select
    /// copy of this text would paste `henry.fitzgerald@<U+200B>example.com`
    /// — a credential-adjacent string silently broken by an invisible
    /// character, with nothing on screen explaining why a login form
    /// rejected it. That is why `.textSelection` is deliberately OFF this
    /// `Text` (do not re-enable it without solving that first). The
    /// sanctioned way to get the real address is `.help(account.name)` on
    /// hover, or the row's own `Button("Copy Account Name")`, both of which
    /// read `account.name` directly and never see this hint.
    private var emailWithBreakHint: String {
        let email = account.ref.displayHalves.email
        guard let atIndex = email.firstIndex(of: "@") else { return email }
        let afterAt = email.index(after: atIndex)
        return email[..<afterAt] + "\u{200B}" + email[afterAt...]
    }

    private var information: some View {
        VStack(alignment: .leading, spacing: Tok.rowLineSpacing) {
            HStack(spacing: Tok.tightSpacing) {
                // The EMAIL HALF only. The `/<org-slug>` half is a tag on the
                // designations line below — see `orgIndicator`.
                //
                // This line used to carry the whole name and middle-truncate
                // it, which on a qualified name rendered `henry@ex…ample-team`:
                // not readable, not typable, and truncating the one thing that
                // says which of a person's orgs the row is. Splitting the two
                // halves across two lines is what lets BOTH be shown whole,
                // rather than choosing which end to sacrifice.
                //
                // `.help` deliberately stays the FULL name, not this half: the
                // tooltip is the "what do I type" answer, and the answer is the
                // whole thing.
                // WRAPS rather than truncates. Splitting the org half off is
                // not on its own enough: line one also carries the live-state
                // pills, every one of them `fixedSize()`, so on a row wearing
                // two of them a perfectly ordinary address still ran out of
                // room — `henry.mitchell@examp…` with ROTATING + UNMEASURED
                // beside it. The pills cannot yield, so the name has to be
                // allowed a second line instead of losing its tail.
                //
                // `fixedSize(horizontal:vertical:)` is what makes the wrap
                // real: without it the `Text` reports the one-line height it
                // was offered and clips, rather than growing.
                // No `.textSelection` here — see ``emailWithBreakHint``: this
                // string carries an invisible break hint a drag-select copy
                // would paste verbatim. `.help` below and "Copy Account
                // Name" are the sanctioned ways to get the real address.
                Text(emailWithBreakHint)
                    // v4-spec: account name is 15pt/600/-0.005em, system
                    // design — `Tok.bodyFont` is 13pt/medium/rounded, a
                    // token owned by `feat/tokens-parity`; literal here per
                    // the coordinator's fence until that lane's name lands.
                    .font(Tok.nameFont)
                    .tracking(-0.005 * 15)
                    .lineSpacing(Tok.bodyLineSpacing)
                    .foregroundStyle(account.disabled ? Tok.disabled : Tok.ink)
                    .lineLimit(2)
                    .fixedSize(horizontal: false, vertical: true)
                    .help(account.name)
                controlIndicator
                Spacer(minLength: Tok.tightSpacing)
                rotationPill
                // A never-probed account's `quotaState` is Rust's default, not a
                // reading. Printing `ok` on it would be the panel asserting
                // something nothing has ever checked.
                //
                // Three cases, not two. "No quota reading" has two causes and
                // they are not interchangeable: nothing has asked yet, or the
                // asking failed. Both used to render UNMEASURED, so an account
                // whose probe errored was labelled with the one word this
                // palette reserves for *never probed* — telling the operator to
                // wait for a sweep that had already run and failed. Observed
                // live: a row reading UNMEASURED beside a status of `error`.
                // Broken beats every other case, including a stale
                // "never probed" read: `status == "error"` paired with
                // `probeStatus == .never` is the case actually occurring on
                // the live fleet, and it is a known cause with a known remedy,
                // not an absence of information.
                if account.health == .needsRelogin {
                    StatusPill("needs re-login", tint: Tok.spent)
                        .help(
                            "The refresh token was rejected — this account is out "
                                + "of rotation and serves no traffic until you re-login."
                        )
                } else if account.isRejected {
                    // Ahead of the quota cases deliberately. A rejected account
                    // can carry a perfectly ordinary reading — the gate is
                    // Anthropic's, not a quota fact — so `spent`/`ok` would be
                    // a true number attached to a false conclusion about
                    // whether traffic can land here. The cause outranks the
                    // measurement, the same way `needs re-login` above does.
                    StatusPill("rejected", tint: Tok.spent)
                        .help(
                            "Anthropic has rejected this account, so `tcr` sends it no "
                                + "traffic whatever its quota reads. It returns on their "
                                + "verdict, not on a reset."
                        )
                } else if account.hasQuotaEvidence {
                    StatusPill(account.quotaState.token, tint: quotaTint)
                } else if account.probeStatus.isFailure {
                    // The probe's own word, so the row names the cause rather
                    // than a category. Still `Tok.unmeasured`: "we have no
                    // reading" is the true part and stays in the cool,
                    // off-the-traffic-light hue — a failed probe is not a
                    // measured exhaustion and must not read as one.
                    StatusPill(account.probeStatus.token, tint: Tok.unmeasured)
                        .help(
                            account.probeError.map { "Quota probe failed: \($0)" }
                                ?? "The quota probe ran and failed "
                                + "(\(account.probeStatus.token)) — this account's "
                                + "quota is unknown."
                        )
                } else {
                    StatusPill("unmeasured", tint: quotaTint)
                        .help("Never probed — this account's quota is unknown, not zero.")
                }
            }
            // Covers `controlIndicator` and `rotationPill` fading in or out on
            // this line. `account` catches everything `rotationPill` reads;
            // `controlIndicator` reads `control`, a separate `@ObservedObject`,
            // so it needs its own binding — `account` changing says nothing
            // about which account is held out as the control one.
            .animation(reduceMotion ? nil : Tok.standardAnimation, value: account)
            .animation(
                reduceMotion ? nil : Tok.standardAnimation,
                value: control.isControl(account.name))
            designationsLine
            // Two window lines, 5-hour on top and 7-day directly under it —
            // Gil's explicit call (bridge, 2026-08-18) — each tinted by its OWN
            // window's state (`fiveHourTint`/`sevenDayTint`) rather than the
            // shared gating tint the single bar used to wear: a 7d-red account
            // with an empty 5h window must not paint its 5h bar red. The
            // composite `quota`/`quotaState` fields are UNCHANGED on the wire
            // and still drive the status pill above — only this bar+percentage
            // display stops using them, since with both windows drawn and
            // numbered here, a third "most-spent-of-both" percentage would be
            // the same fact restated rather than new information.
            //
            // Label, bar and percentage share ONE line per window, which took
            // the card from 96pt to 59pt. The earlier choice here was label
            // above its bar versus below it; above won because below read as
            // bar / label / bar / label with nothing saying which label went
            // with which. Beside cannot be misread at all — a label and its bar
            // are on the same line. The reset caption comes with it, so the
            // hourglass line that said the same thing lower down is gone.
            //
            // The extra top padding is asymmetric on purpose: at
            // `Tok.rowLineSpacing` alone this first line collided with the
            // pills, which carry their own vertical padding.
            quotaLine(
                window: "5h",
                fraction: account.fiveHour,
                tint: fiveHourTint,
                barLabel: "5-hour quota",
                resetAtMs: account.fiveHourResetAtMs,
                captionTint: captionTint(for: .fiveHour)
            ) {
                // The spend/output-tokens figure that used to sit here moved
                // to the plan line (`designationsLine` → `planIndicator`),
                // per `docs/design/panel-tabs-mockup.html`'s plan line
                // ("Max 20x · $540 · 1.5M output tokens this week") and the
                // panel-parity finding that this column should carry one
                // meaning — the reset countdown — on every row. See
                // `planIndicator`'s doc-comment for where it draws now.
                EmptyView()
            }
            .padding(.top, Tok.space1)
            quotaLine(
                window: "7d",
                fraction: account.sevenDay,
                tint: sevenDayTint,
                barLabel: "7-day quota",
                resetAtMs: account.sevenDayResetAtMs,
                captionTint: captionTint(for: .sevenDay)
            ) {
                // `status` is the account's own field and it keeps saying
                // "active" while `disabled` is true — verified against live
                // output, not just a fixture. Printing it next to a PARKED pill
                // puts "parked" and "active" in one line and makes the row argue
                // with itself, so the pill speaks for a parked account and the
                // raw status only shows when it can be true. `active` is dropped
                // too: the ROTATING pill already says it. The word now prints
                // only when it adds something — `throttled`, `error`, whatever
                // else `tcr` reports.
                if !account.disabled && account.status != "active" {
                    Text(account.status)
                        .font(Tok.secondaryFont).lineSpacing(Tok.secondaryLineSpacing)
                        // `active` and `error` must not be pixel-identical: an
                        // `error` account is what the UNMEASURED pill used to
                        // wear too, and the raw word alone drew in the same
                        // grey as a healthy account right above it.
                        .foregroundStyle(account.health == .needsRelogin ? Tok.spent : Tok.inkDim)
                        .lineLimit(1)
                }
                // The FABLE weekly window — a third quota window, with its own
                // reset, that gates Fable requests only and is reflected in
                // neither the `7d` bar beside it nor `held[]`/`quotaState`
                // (`docs/cli.md`, "The weekly quota pair on `--json`"). Until
                // this slot existed the panel drew no Fable figure at all
                // while the router was already gating on one.
                //
                // Absent, never `n/a`, when the server did not measure it:
                // `fableWeeklyLabel` is nil for an older server or a
                // never-learned window and this slot stays empty, the same
                // rule the 5h line's spend figures follow.
                if let fable = account.fableWeeklyLabel {
                    Text(fable)
                        .font(Tok.detailDigitFont).lineSpacing(Tok.detailLineSpacing)
                        .foregroundStyle(fableFigureStyle)
                        .lineLimit(1)
                }
            }
            // The serving counters this line used to print. A running total
            // since the proxy started answers "has this account been used",
            // which is read once; the Fable percentage above is a live gate on
            // what the router will do next, and only one of the two fits in
            // the slot. Absent — no tooltip at all — on a structurally zero
            // offline read, the same rule the printed line followed.
            .help(account.countersTooltip(countersAreStructural: countersAreStructural) ?? "")
            // A nil count here is not a quantity with an unknown value —
            // unlike `account.requests` above, this number is a MODIFIER
            // on the error string that is already being displayed, and
            // the error alone is the actionable fact regardless of how
            // many times it happened. See
            // `QuotaFormat.streamErrorLabel(count:error:)`'s doc comment
            // for why this suppresses the multiplier on `nil` instead of
            // following `QuotaFormat.count`'s "n/a" the way the line
            // above does, and returns `nil` outright on a MEASURED zero —
            // a count of 0 means the error has not recurred, and the line
            // must not render at all.
            if let error = account.lastStreamError, !error.isEmpty,
                let label = QuotaFormat.streamErrorLabel(count: account.streamErrorCount, error: error)
            {
                Text(label)
                    .font(Tok.detailFont).lineSpacing(Tok.detailLineSpacing)
                    .foregroundStyle(Tok.spent)
                    .lineLimit(2)
            }
            if let failure = accounts.failure(for: account.ref) {
                // `tcr`'s own words, verbatim. A toggle that did not happen must
                // never be indistinguishable from one that did.
                Label(failure.summary, systemImage: Tok.unreadableGlyph)
                    .font(Tok.detailFont).lineSpacing(Tok.detailLineSpacing)
                    .foregroundStyle(Tok.spent)
                    .fixedSize(horizontal: false, vertical: true)
            }
            if let failure = removeController.failure(for: account.ref) {
                Label(failure.summary, systemImage: Tok.unreadableGlyph)
                    .font(Tok.detailFont).lineSpacing(Tok.detailLineSpacing)
                    .foregroundStyle(Tok.spent)
                    .fixedSize(horizontal: false, vertical: true)
            }
            if let failure = tokenCopyFailure {
                Label(failure.summary, systemImage: Tok.unreadableGlyph)
                    .font(Tok.detailFont).lineSpacing(Tok.detailLineSpacing)
                    .foregroundStyle(Tok.spent)
                    .fixedSize(horizontal: false, vertical: true)
            }
            // A refused group edit, drawn like every other refusal on this row.
            // It was recorded and never read: `GroupController` has stored these
            // all along and nothing displayed them, so a `tcr group add` that
            // exited non-zero — which is what an ambiguous name does on a fleet
            // holding one email twice — was indistinguishable from one that
            // worked. The group is named because the menu that started it has
            // closed by the time this appears.
            if let refused = groupController.failure(forAccount: account.ref) {
                Label(
                    "group '\(refused.group)': \(refused.failure.summary)",
                    systemImage: Tok.unreadableGlyph
                )
                .font(Tok.detailFont).lineSpacing(Tok.detailLineSpacing)
                .foregroundStyle(Tok.spent)
                .fixedSize(horizontal: false, vertical: true)
            }
            verdictLine
            removalNoticeLine
        }
    }

    /// One window of quota on one line — `5h ▓▓▓░░░ 12% in 2h 10m` — then
    /// whatever the caller puts on the right.
    ///
    /// Label and bar are fixed-width, so every card's percentage lands in the
    /// same column. That column is what makes thirteen rows scannable.
    ///
    /// `hasStaleQuotaReading` demotes label, bar, percentage and caption
    /// together: a row must not read half live and half historical.
    @ViewBuilder
    private func quotaLine<Trailing: View>(
        window: String,
        fraction: Double?,
        tint: Color,
        barLabel: String,
        resetAtMs: Int64?,
        captionTint: Color,
        @ViewBuilder trailing: () -> Trailing
    ) -> some View {
        HStack(spacing: Tok.tightSpacing) {
            Text(window)
                .font(Tok.secondaryDigitFont).lineSpacing(Tok.secondaryLineSpacing)
                .foregroundStyle(hasStaleQuotaReading ? Tok.disabled : Tok.inkFaint)
                .frame(width: Tok.windowLabelWidth, alignment: .leading)
            QuotaBar(fraction: fraction, tint: tint, label: barLabel)
                // Sighted-hover half of the same fix `quotaTint`/`fiveHourTint`
                // make for the fill colour: a muted bar alone can still read as
                // "just a dim healthy bar" rather than "not live" without a word
                // saying so on hover.
                .help(
                    hasStaleQuotaReading
                        ? "The last reading taken before the credential was rejected. "
                            + "This headroom is unreachable until you re-login."
                        : ""
                )
            Text(QuotaFormat.percent(fraction))
                .font(Tok.secondaryDigitFont).lineSpacing(Tok.secondaryLineSpacing)
                // Demoted alongside the bar, not left `.secondary`: the eye
                // should group these digits as historical rather than live, the
                // same distinction `fiveHourTint` draws for the fill. The number
                // itself is unchanged — it is still true, just no longer
                // reachable.
                .foregroundStyle(hasStaleQuotaReading ? Tok.disabled : Tok.inkDim)
            // Drawn whenever the wire has a reset for this window, beside the
            // number it belongs to. Colour: `captionTint(for:)`. `resetsCaption`,
            // not `resetCaption`: this column is the grid's one meaning now
            // that the spend figure that used to compete with it for the row's
            // width moved onto the plan line (`designationsLine`) — see that
            // formatter's own doc-comment.
            if let caption = QuotaFormat.resetsCaption(resetAtMs: resetAtMs, now: Date()) {
                Text(caption)
                    // Tabular, like the percentage it sits beside: this string
                    // carries digits (`in 4d 12h`) that change under the poll,
                    // and proportional figures shift the row when they do.
                    .font(Tok.secondaryDigitFont).lineSpacing(Tok.secondaryLineSpacing)
                    .foregroundStyle(captionTint)
                    .lineLimit(1)
            }
            Spacer(minLength: Tok.tightSpacing)
            trailing()
        }
    }

    /// Drawn after a successful ``RemoveAccountController/remove(account:org:)``
    /// and never cleared while the panel stays open — the whole reason it
    /// exists. `remove_account` now disables the account in the running proxy
    /// before deleting it from the config, so the account itself stops
    /// serving immediately — but this row keeps rendering exactly as it did
    /// before, because nothing here re-derives the fleet from a config the
    /// running proxy has not re-read, and the row's membership in the fleet
    /// list is itself a boot-time snapshot (see ``RemoveAccountControl``'s
    /// doc-comment). A silent success here is the exact bug this codebase
    /// already lost an evening to for the enable/disable toggle — so this
    /// line says what is actually true: stopped now, listed as disabled until
    /// the next restart. It must not claim the row will disappear, because it
    /// will not.
    ///
    /// `Tok.near`, matching `verdictLine`'s `.notHonoured`/`.spokeUp` tint:
    /// nothing failed — the delete landed — so this must not wear the error
    /// colour the line above reserves for a `tcr` failure.
    @ViewBuilder
    private var removalNoticeLine: some View {
        if removeController.needsRestart(account.ref) {
            Label(
                "Removed from config and stopped. Stays listed as disabled until the proxy restarts.",
                systemImage: "arrow.triangle.2.circlepath"
            )
            .font(Tok.detailFont).lineSpacing(Tok.detailLineSpacing)
            .foregroundStyle(Tok.near)
            .fixedSize(horizontal: false, vertical: true)
        }
    }

    /// The outcome of the last successful toggle, as the fleet then reported it.
    ///
    /// Three tints for three different facts. A not-honoured toggle is drawn in
    /// `Tok.near` rather than `Tok.spent`: nothing failed — the call landed and
    /// the config changed — so it must not wear the colour this palette reserves
    /// for errors, which is already taken by the verbatim `tcr` failure directly
    /// above it. The two lines can appear for different reasons and have to stay
    /// tellable apart.
    @ViewBuilder
    private var verdictLine: some View {
        if let verdict = accounts.verdict(for: account.ref, reportedDisabled: account.disabled) {
            Label(verdict.rowLabel, systemImage: Self.verdictGlyph(verdict))
                .font(Tok.detailFont).lineSpacing(Tok.detailLineSpacing)
                .foregroundStyle(Self.verdictTint(verdict))
                .fixedSize(horizontal: false, vertical: true)
        }
    }

    private static func verdictGlyph(_ verdict: ToggleVerdict) -> String {
        switch verdict {
        case .confirmed: return "checkmark.circle"
        case .notHonoured: return Tok.unreadableGlyph
        case .unverified: return "questionmark.circle"
        // A `tcr` notice never wears the tick, whatever it qualifies — including a
        // confirmation. `checkmark.circle` beside "the fleet now reports parked"
        // would put the glyph an operator scans for on a park that may not survive
        // a restart, which is the whole defect.
        case .spokeUp: return Tok.unreadableGlyph
        }
    }

    private static func verdictTint(_ verdict: ToggleVerdict) -> Color {
        switch verdict {
        case .confirmed: return Tok.ok
        case .notHonoured: return Tok.near
        case .unverified: return Tok.unmeasured
        // `Tok.near`, sharing the not-honoured hue for the same reason: nothing
        // failed — `tcr` exited 0 — so it must not take the error colour already
        // spoken for by the verbatim failure line above, and it must not take
        // `Tok.ok`, which is the clean case's alone.
        case .spokeUp: return Tok.near
        }
    }

    /// Hands `tcr login --account <name>` to a Terminal window, targeting THIS
    /// row's account. Drawn on a `.needsRelogin` row only, beside
    /// ``toggleButton``.
    ///
    /// `--account` (`src/main.rs` / `src/oauth.rs`'s `login_hint`) requests
    /// that specific identity and refuses to write anything if the browser
    /// hands back a different one — this app used to say `tcr login` could
    /// not be targeted at all, which stopped being true the moment that flag
    /// shipped. Untargeted, the row's click could authenticate as whichever
    /// account happened to be signed into the browser; targeted, a mismatch
    /// refuses rather than overwriting the wrong account's credentials.
    private var reloginButton: some View {
        Button("Re-login…") { onRelogin() }
            .buttonStyle(.bordered)
            .controlSize(.small)
            .font(Tok.detailFont).lineSpacing(Tok.detailLineSpacing)
            .accessibilityLabel("Re-login \(account.name)")
            .help(
                "Opens `tcr login --account` in a Terminal window, requesting "
                    + "this exact account. `tcr` refuses to save if the browser "
                    + "hands back a different one."
            )
    }

    /// `tcr enable <name>` / `tcr disable <name>`, keyed off the account's own
    /// `disabled` field so the label always names what the click will do.
    private var toggleButton: some View {
        let enabling = account.disabled
        let pending = accounts.isPending(account.ref)
        // A bare "…" names neither the action nor its progress, and a screen
        // reader announces it as punctuation. The button is already disabled
        // while in flight, so its label is the only signal anything is happening.
        let verb = enabling ? "Enable" : "Disable"
        let title = pending ? (enabling ? "Enabling…" : "Disabling…") : verb
        return Button(title) {
            Task { await performToggle(enabling: enabling) }
        }
        // `.bordered`, not `.borderless`. Borderless draws the label as bare
        // text, so the ONE actionable control in the row looked exactly like
        // the two informational pills beside it — thirteen rows of a word that
        // reads as a status and is actually a button. It survived review
        // because `ImageRenderer` cannot draw AppKit controls at all: every
        // `--render-states` PNG shows a placeholder here, so no snapshot could
        // have caught it. It took a screenshot of the running app.
        //
        // `.small` keeps the added chrome from competing with the account name,
        // which is still the thing being scanned for.
        .buttonStyle(.bordered)
        .controlSize(.small)
        .font(Tok.detailFont).lineSpacing(Tok.detailLineSpacing)
        .disabled(pending)
        // Thirteen rows otherwise render thirteen controls whose entire
        // accessible name is "Disable", with nothing to say which account is
        // about to leave rotation. `.help` is a tooltip, not an accessible name.
        .accessibilityLabel("\(verb) \(account.name)")
        .help(
            enabling
                ? "Run `tcr enable` for this account and re-read the fleet."
                : "Run `tcr disable` to take this account out of rotation. Reversible."
        )
    }

}

/// A clamped quota bar. Over-100% clamps visually but the numeric label beside it
/// still shows the real figure.
///
/// `fraction` is optional because a never-probed account has no reading at all.
/// A nil renders as a DASHED outline rather than an unfilled bar: an unfilled bar
/// is pixel-identical to `0`, and since the fraction is utilization, drawing one
/// would make the panel assert "nothing spent, full headroom" about an account
/// nothing has ever measured. Keeping unknown distinct from empty at the last
/// rendering step is the whole point of the optional model — collapsing it here
/// would undo it after the decoder, `readyCount` and the pill all took care to
/// preserve it.
struct QuotaBar: View {
    let fraction: Double?
    let tint: Color
    /// Which window this bar speaks for — "Quota" (the pre-existing composite
    /// bar) or "5-hour quota" / "7-day quota" for the two per-window bars this
    /// row now draws. Named so VoiceOver reading two bars in a row hears two
    /// distinct labels rather than "Quota" twice.
    var label: String = "Quota"
    /// Fixed, not whatever the line leaves over: a bar that filled the leftover
    /// width would be a different length on every card. See `Tok.barWidth`.
    var width: CGFloat = Tok.barWidth

    /// Honour the system Reduce Motion setting. SwiftUI does not suppress an
    /// explicit `.animation` for you.
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    /// What assistive technology hears.
    ///
    /// Without this the bar is a nameless graphic, so the panel's single most
    /// important number is invisible to VoiceOver — and worse, the nil-vs-zero
    /// distinction that the decoder, `readyCount` and the pill all take care to
    /// preserve would survive only as a dashed outline. "Never measured" and
    /// "nothing spent" would be identical again, which is the exact defect the
    /// optional model exists to prevent.
    private var spokenValue: String {
        guard let fraction else { return "not measured" }
        return "\(Int((fraction * 100).rounded())) percent used"
    }

    var body: some View {
        ZStack(alignment: .leading) {
            RoundedRectangle(cornerRadius: Tok.barRadius).fill(Tok.track)
            if let fraction {
                RoundedRectangle(cornerRadius: Tok.barRadius)
                    .fill(tint)
                    .frame(width: width * min(max(fraction, 0), 1))
            } else {
                RoundedRectangle(cornerRadius: Tok.barRadius)
                    .strokeBorder(tint, style: StrokeStyle(lineWidth: 1, dash: [2, 2]))
            }
        }
        // v4-spec: quota bar height is 7pt (`Tok.barHeight` is 6) — literal
        // per the tokens-parity fence.
        .frame(width: width, height: 7)
        .animation(reduceMotion ? nil : Tok.standardAnimation, value: fraction)
        // A second binding, not a merged one: `fraction` and `tint` change
        // independently (a re-poll moves the fill; a band crossing recolours
        // it) and SwiftUI animates each `.animation(_:value:)` on its own
        // value, so both ease without either gating the other.
        .animation(reduceMotion ? nil : Tok.standardAnimation, value: tint)
        .accessibilityElement()
        .accessibilityLabel(label)
        .accessibilityValue(spokenValue)
    }
}
