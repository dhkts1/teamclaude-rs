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

    /// Surfaced in place rather than swallowed: a button that silently does
    /// nothing is worse than one that says why.
    @State private var loginError: String?

    /// Measured height of each account row, keyed by account id (`Account.id`
    /// is the account name).
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
    /// Per-row rather than one summed total because rows are not uniform height
    /// — the needs-relogin state and several conditional detail lines all grow a
    /// row. `visibleRowsHeight(for:)` sums all of these and clamps the total to
    /// `Tok.panelMaxHeight`.
    @State private var rowHeights: [String: CGFloat] = [:]

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
    }

    // MARK: Header

    private var header: some View {
        VStack(alignment: .leading, spacing: Tok.tightSpacing) {
            HStack {
                Text("tcr fleet").font(.headline)
                Spacer()
                if let at = poller.lastPollAt {
                    Text(at, style: .time)
                        .font(Tok.secondaryDigitFont)
                        .foregroundStyle(.secondary)
                }
            }
            // Only when the read is NOT healthy. On a healthy read this said
            // "13 accounts — live", which the tallies below say with more
            // detail. Every other state — pending, tool missing, command
            // failed, undecodable, stale — is carried nowhere else in the
            // panel, so it still prints, and it is then the most important
            // sentence on screen.
            if !poller.state.isHealthyRead {
                Text(poller.state.summary)
                    .font(Tok.secondaryFont)
                    .fixedSize(horizontal: false, vertical: true)
            }
            if case .loaded(let fleet) = poller.state, !fleet.accounts.isEmpty {
                capacitySummary(fleet)
            }
            usageSummary
            updateStateLine
        }
    }

    /// Where the fleet's spend, burn rate, model mix and cache hit rate go, once
    /// the proxy puts a `usage` object on the wire. Empty until then: a row of
    /// zeros would answer a question the panel cannot answer yet. Named so the
    /// next change has a place to fill rather than a header to re-derive.
    @ViewBuilder
    private var usageSummary: some View {
        EmptyView()
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
                .font(Tok.secondaryFont)
                .foregroundStyle(isFailure ? Tok.spent : Tok.accent)
                .fixedSize(horizontal: false, vertical: true)
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
        var line =
            Text(fleet.capacitySummary)
            .font(.subheadline.weight(.semibold))
            .foregroundColor(Tok.color(for: fleet.capacityState))
        for tally in fleet.breakdown {
            line =
                line
                + Text(" · ").font(Tok.secondaryFont).foregroundColor(Tok.inkFaint)
                + Text(tally.label)
                .font(Tok.secondaryDigitFont)
                .foregroundColor(Tok.color(for: tally.kind))
        }
        return
            line
            .fixedSize(horizontal: false, vertical: true)
            .accessibilityLabel("\(fleet.capacitySummary), \(fleet.breakdownLabel)")
            .padding(.top, Tok.tightSpacing)
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
                detail: "Searched \(searched.count) locations. Set it with "
                    + "`defaults write io.github.dhkts1.tcrbar \(TcrTool.overrideDefaultsKey) <path>`.",
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
            banner(
                icon: Tok.unreadableGlyph,
                title: "Unreadable status output",
                detail: message,
                tint: Tok.unknown
            )
        case .loaded(let fleet):
            if fleet.accounts.isEmpty {
                banner(
                    icon: Tok.unreadableGlyph,
                    title: "No accounts configured",
                    detail: "tcr answered, and the fleet is empty.",
                    tint: Tok.unknown
                )
            } else {
                fleetRows(fleet)
            }
        }
    }

    private func fleetRows(_ fleet: Fleet) -> some View {
        VStack(alignment: .leading, spacing: Tok.rowSpacing) {
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
                .onPreferenceChange(RowHeightsKey.self) { rowHeights = $0 }
            }
        }
    }

    /// A flat list of accounts — the only shape this panel draws now. Group
    /// membership shows as a tag on the row (``AccountRow``'s pills line),
    /// not as a section, card or separate view; see the bridge for why three
    /// earlier attempts at the latter were all rejected.
    private func accountList(_ fleet: Fleet) -> some View {
        let rows = fleet.rowsInDisplayOrder(pinning: control.current)
        return VStack(alignment: .leading, spacing: Tok.rowSpacing) {
            ForEach(Array(rows.enumerated()), id: \.element.id) { index, account in
                accountRow(account, fleet: fleet)
                // A thin separator between the pinned control row and the
                // rotation pool below it — the same `Hairline` this panel
                // already uses to mark a scope boundary (see `appActions`).
                // Only drawn when the control row is actually first: index 0
                // is an ordinary pool row on every fleet without one set.
                if index == 0, account.name == control.current {
                    Hairline()
                }
            }
        }
    }

    private func accountRow(_ account: Account, fleet: Fleet) -> some View {
        AccountRow(
            account: account,
            countersAreStructural: fleet.source.countersAreStructural,
            accounts: accounts,
            control: control,
            onChanged: { await poller.pollOnce() },
            onRelogin: { reloginAccount(account.name) },
            groupController: groupController,
            removeController: removeController,
            allAccounts: fleet.accounts,
            snapshotMode: snapshotMode
        )
        .background(
            GeometryReader { proxy in
                Color.clear.preference(
                    key: RowHeightsKey.self, value: [account.id: proxy.size.height])
            }
        )
    }

    /// Height of the scroll viewport: every row, clamped to `Tok.panelMaxHeight`.
    ///
    /// It used to stop at the first four rows, which left the panel short of its
    /// own cap while it had more to show — and a row count is the wrong unit
    /// anyway, since rows are not uniform height. A two-account fleet now draws
    /// in full and a thirteen-account one fills to 520pt and scrolls.
    ///
    /// Fallback: before SwiftUI has laid out and reported any row height (the
    /// first frame, prior to the first `onPreferenceChange`), `rowHeights` is
    /// empty and this returns `Tok.panelMaxHeight`, so the panel never renders
    /// at zero or one-row height while waiting for a real measurement.
    private func visibleRowsHeight(for fleet: Fleet) -> CGFloat {
        let orderedHeights = fleet.rowsInDisplayOrder(pinning: control.current).compactMap {
            rowHeights[$0.id]
        }
        guard !orderedHeights.isEmpty else {
            return Tok.panelMaxHeight
        }
        let summed =
            orderedHeights.reduce(0, +)
            + Tok.rowSpacing * CGFloat(max(orderedHeights.count - 1, 0))
        return min(max(summed, Tok.rowSpacing), Tok.panelMaxHeight)
    }

    private func offlineNotice(_ source: StatusSource) -> some View {
        HStack(spacing: Tok.tightSpacing) {
            Image(systemName: "wifi.slash")
            Text("source: \(source.token) — quota is real, all serving counters are structurally zero.")
                .fixedSize(horizontal: false, vertical: true)
        }
        .font(Tok.secondaryFont)
        .foregroundStyle(Tok.offline)
    }

    private func banner(icon: String, title: String, detail: String, tint: Color) -> some View {
        HStack(alignment: .top, spacing: Tok.gutter) {
            Image(systemName: icon).foregroundStyle(tint)
            VStack(alignment: .leading, spacing: Tok.tightSpacing) {
                Text(title).font(.subheadline.weight(.semibold))
                Text(detail)
                    .font(Tok.secondaryFont)
                    .foregroundStyle(.secondary)
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
                Text(server.state.summary)
                    .font(Tok.secondaryFont)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
                Spacer(minLength: Tok.tightSpacing)
                if case .loaded(let fleet) = poller.state, let sha = fleet.serverSha {
                    Text("server \(sha)\(fleet.serverDirty ? "-dirty" : "")")
                        .font(Tok.detailDigitFont)
                        .foregroundStyle(.tertiary)
                        .lineLimit(1)
                }
            }
            fleetActions
            // Scope boundary by PROXIMITY, not by a rule. A `Hairline` shipped
            // here first and was then rendered with `--render-states`: it put a
            // THIRD full-width rule into the bottom third of the panel — one
            // above this block, one here, one above the danger zone — and the
            // new one sat between two rows that are both just buttons, while
            // the checkboxes directly below it got no separator at all. The
            // original trailing-alignment was reaching for the right thing
            // ("without needing a rule between them"); only its method was
            // wrong. Space groups these two without adding weight.
            appActions
                .padding(.top, Tok.tightSpacing)

            if let loginError {
                Text(loginError)
                    .font(Tok.detailFont)
                    .foregroundStyle(Tok.spent)
                    .fixedSize(horizontal: false, vertical: true)
            }

            launchAtLogin
            startServerToggle
            keepAwakeToggle

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

    /// Actions that act on the APP rather than on the fleet.
    ///
    /// These used to be trailing-aligned, on the reasoning that opposite
    /// alignment would read as a separate group "without needing a rule between
    /// them". In practice it read as misalignment: two button rows with
    /// different left edges look broken before they look grouped, and the
    /// second row's buttons floated away from everything above them. The scope
    /// split is real, so it is now drawn — a `Hairline`, the same divider this
    /// panel already uses to separate its sections — and both rows share one
    /// left edge so the footer has a single vertical rhythm.
    private var appActions: some View {
        HStack(spacing: Tok.tightSpacing) {
            // Disabled rather than silently no-op while Sparkle already has
            // a check in flight — the same rule "Take over port…" follows.
            Button("Check for Updates…") { updater.checkForUpdates() }
                .disabled(!updater.canCheckForUpdates)
                .help(
                    "Ask the release feed whether a newer TcrBar exists. "
                        + "Also reachable as `tcrbar://check-for-updates`."
                )
            Button("Quit") { NSApplication.shared.terminate(nil) }
            Spacer(minLength: 0)
        }
        .buttonStyle(.bordered)
    }

    /// Bring the proxy up when TcrBar starts. Pairs with "Launch at login" to
    /// mean "the proxy is always up".
    ///
    /// The warning is not decoration. Once TcrBar supervises the server, Quit
    /// stops it — correct for a supervisor, and a genuinely expensive surprise if
    /// nobody said so before the box was ticked.
    ///
    /// So it is drawn UNCONDITIONALLY, not inside `if startServerAtLaunch`. The
    /// preference defaults to off (`LaunchPreference.swift`), so a caveat that
    /// only appears once the box is ticked appears strictly *after* the decision
    /// it exists to inform — it can tell an operator what they already did, never
    /// what they are about to do. The hover help cannot carry it either: a
    /// tooltip is opt-in, and this cost is not.
    ///
    /// Always-present also holds the panel's height still as the box toggles,
    /// which a conditional line does not: the fleet rows above would shift under
    /// the pointer at the moment of the click.
    ///
    /// It carries `Tok.inkFaint`, not `Tok.near`. Every amber line in this footer
    /// is a live condition of *this* install; this one is a standing fact about
    /// what the mode means, true before anybody chooses anything, and drawing a
    /// permanent note in the alarm colour would leave the panel looking alarmed at
    /// rest. The wording is deliberately state-neutral for the same reason — it
    /// reads correctly both as a consequence to weigh and as one already taken on.
    private var startServerToggle: some View {
        VStack(alignment: .leading, spacing: Tok.tightSpacing) {
            Toggle("Start server at launch", isOn: $startServerAtLaunch)
                .toggleStyle(.checkbox)
                .font(Tok.secondaryFont)
                .help(
                    "Runs `tcr server --headless --no-replace` when TcrBar starts. "
                        + "`--headless` is the load-bearing one: it keeps the "
                        + "server alive with no terminal to run its TUI in. "
                        + "Standing down rather than disturbing a proxy that is "
                        + "already serving is the default, which `--no-replace` "
                        + "only restates for an older `tcr`. TcrBar then "
                        + "supervises the server it started, and quitting TcrBar "
                        + "stops it."
                )
            Text("TcrBar supervises a server it starts, so quitting TcrBar stops it.")
                .font(Tok.detailFont)
                .foregroundStyle(Tok.inkFaint)
                .fixedSize(horizontal: false, vertical: true)
        }
    }

    /// Keep the Mac from idle-sleeping, for as long as the box is ticked.
    ///
    /// A checkbox rather than a fifth button: the button row above is already
    /// four wide inside a 380pt panel, and this belongs with the other two
    /// toggles anyway — all three are modes, not actions.
    ///
    /// The detail line is not decoration. "Keep this Mac awake" over-promises by
    /// exactly the two cases an operator will hit — a dark screen, and a laptop
    /// on battery, where the `PreventSystemSleep` half of the hold is inert per
    /// `man caffeinate` — and hitting either means coming back to a dead run and
    /// blaming the proxy. It says nothing about a closed lid in either
    /// direction, because nothing here has measured that.
    ///
    /// It carries `Tok.awake`, NOT `Tok.near`. Amber is this palette's "close to
    /// a gating limit", and it is what the login-item error directly above uses;
    /// an informational note about a mode the operator just turned on is not
    /// that, and rendering it in the alarm colour made a footer with one note
    /// read as a footer with two problems. `Tok.awake` is the mode's own token —
    /// the same one the menu-bar mark uses — so the line reads as belonging to
    /// the thing that is on, which is what it is.
    ///
    /// `.tint(Tok.awake)` asks for the mark to be the same token the menu bar
    /// draws, so the two surfaces cannot disagree about what "on" looks like.
    /// **That one line is unverified**, and it is the only thing here that is: a
    /// `.checkbox` toggle is an AppKit control, `ImageRenderer` does not draw
    /// those at all (`--render-states` shows a placeholder for this toggle and
    /// for the two above it, all three the same), and reading the real control
    /// back needs a screenshot. On macOS a checkbox may well follow the system
    /// accent colour and ignore the tint outright. The state is carried by the
    /// checkbox being *ticked*, which is not a colour, so nothing depends on it.
    private var keepAwakeToggle: some View {
        VStack(alignment: .leading, spacing: Tok.tightSpacing) {
            Toggle(
                "Keep this Mac awake",
                isOn: Binding(get: { awake.isOn }, set: { awake.setOn($0) })
            )
            .toggleStyle(.checkbox)
            .font(Tok.secondaryFont)
            .tint(Tok.awake)
            .help(
                "Holds the three power assertions `caffeinate -i -m -s` holds, for "
                    + "as long as this is on. Released when you untick it or quit "
                    + "TcrBar."
            )
            if awake.isOn {
                Text("The display still sleeps. Sleep itself is only held off on AC power.")
                    .font(Tok.detailFont)
                    .foregroundStyle(Tok.awake)
                    .fixedSize(horizontal: false, vertical: true)
            }
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
            switch why {
            case .toolMissing(let searched):
                loginError = "tcr not found (searched \(searched.count) locations)."
            case .couldNotWriteScript(let message):
                loginError = "Could not open Terminal: \(message)"
            }
        } else {
            loginError = nil
        }
    }

    /// Same hand-off as ``addAccount()``, with the account name threaded
    /// through so the Terminal script can name it. Surfaces a failure the same
    /// way — a button that silently does nothing is worse than one that says
    /// why.
    private func reloginAccount(_ name: String) {
        if case .failure(let why) = LoginLauncher.launch(reloggingIn: name) {
            switch why {
            case .toolMissing(let searched):
                loginError = "tcr not found (searched \(searched.count) locations)."
            case .couldNotWriteScript(let message):
                loginError = "Could not open Terminal: \(message)"
            }
        } else {
            loginError = nil
        }
    }

    /// App-level preference, deliberately beside Quit rather than among the
    /// account rows: it is about TcrBar, not about the fleet.
    private var launchAtLogin: some View {
        VStack(alignment: .leading, spacing: Tok.tightSpacing) {
            Toggle(
                "Launch at login",
                isOn: Binding(
                    get: { loginItem.status.isOn },
                    set: { loginItem.set(enabled: $0) }
                )
            )
            .toggleStyle(.checkbox)
            .font(Tok.secondaryFont)
            if let detail = loginItem.status.detail {
                Text(detail)
                    .font(Tok.detailFont)
                    .foregroundStyle(Tok.near)
                    .fixedSize(horizontal: false, vertical: true)
            }
            if let error = loginItem.lastError {
                Text(error)
                    .font(Tok.detailFont)
                    .foregroundStyle(Tok.spent)
                    .fixedSize(horizontal: false, vertical: true)
            }
        }
        .padding(.top, Tok.tightSpacing)
        .onAppear { loginItem.refresh() }
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

/// Carries the measured height of each account row out of the scroll view,
/// keyed by `Account.id`. A dictionary rather than one summed scalar because
/// rows are not uniform height — `visibleRowsHeight(for:)` needs the first N
/// individually, in display order, not just their total.
struct RowHeightsKey: PreferenceKey {
    static let defaultValue: [String: CGFloat] = [:]
    static func reduce(value: inout [String: CGFloat], nextValue: () -> [String: CGFloat]) {
        value.merge(nextValue()) { _, new in new }
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
    /// Hands `tcr login` to a Terminal window for THIS account. Only drawn on a
    /// `.needsRelogin` row.
    let onRelogin: () -> Void
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
    /// Mirrors ``FleetView/snapshotMode``. `ImageRenderer` cannot draw a
    /// `Menu` — it rasterises the yellow "unsupported control" placeholder
    /// the README hero used to ship — so a snapshot draws
    /// ``accountActionsMenuLabel`` on its own, with no `Menu` wrapping it.
    /// Defaults to `false` so every other call site (the live panel) is
    /// unchanged.
    var snapshotMode: Bool = false

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
    /// answer to be no, only two of which this build can currently see:
    /// `disabled` (an operator's own choice), `account.health ==
    /// .needsRelogin` (`src/manager/select.rs:931` hard-excludes an
    /// `AccountStatus::Error` account from selection exactly like a disabled
    /// one, even though `disabled` itself reads false), and a THIRD gate this
    /// pill cannot yet name: `select.rs:809-822` also excludes an account
    /// whose `quota.status == Some("rejected")` — Anthropic's own verdict —
    /// while the snapshot `status` this app decodes stays `"active"`
    /// (`snapshot.rs:142-153` only ever rewrites `Throttled`). Drawing
    /// "rotating" on THAT row is the same "misread as its own opposite"
    /// defect the first two gates were fixed for, and TcrBar currently has no
    /// way to catch it: `tcr status --json` emits no gate field at all. A
    /// server-side `GateReason` in the status payload is the fix, tracked
    /// through the lead rather than added here — decode it as an OPTIONAL
    /// field when it lands, so an older server (absent field) degrades to
    /// today's behaviour and never to a false claim in either direction.
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
    @ViewBuilder
    private var controlIndicator: some View {
        if control.isControl(account.name) {
            StatusPill("control", tint: Tok.accent)
                .help("This account is held out of rotation as the control account.")
        }
    }

    @ViewBuilder
    private var rotationPill: some View {
        if account.disabled {
            StatusPill("parked", tint: Tok.disabled)
                .help("Out of the rotation — `tcr` sends this account no traffic.")
        } else if account.health == .needsRelogin {
            EmptyView()
        } else {
            StatusPill("rotating", tint: Tok.inkFaint)
                .help(
                    "In the rotation — this account can be picked for traffic, as "
                        + "far as this build can tell. A quota rejection from "
                        + "Anthropic can also exclude an account without changing "
                        + "its status; TcrBar cannot see that gate yet."
                )
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
        } else if account.health == .needsRelogin {
            parts.append("not in rotation")
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
        if let failure = accounts.failure(for: account.name) {
            parts.append("last action failed: \(failure.summary)")
        }
        if let failure = removeController.failure(for: account.name) {
            parts.append("last action failed: \(failure.summary)")
        }
        // Spoken for the same reason the pill is: a confirmation only a sighted
        // user gets is half built, and the `✓` in `rowLabel` is punctuation to a
        // screen reader.
        if let verdict = accounts.verdict(for: account.name, reportedDisabled: account.disabled) {
            parts.append(verdict.spokenLabel)
        }
        // Same reason `removalNoticeLine` is drawn at all: a VoiceOver user
        // gets no other signal that the delete landed but the row did not
        // change, since nothing here re-derives the fleet from the config.
        if removeController.needsRestart(account.name) {
            parts.append("removed from config and stopped, stays listed as disabled until the proxy restarts")
        }
        return parts.joined(separator: ", ")
    }

    var body: some View {
        rowContent
            // The card inset. Concentric with the border below it: the corner
            // radius is `Tok.radiusMedium` and this is the padding that keeps
            // that curve from clipping the account name or the toggle button —
            // an outer radius with no matching inset draws a border that bites
            // into its own content at the corners.
            //
            // It is also the only inner padding now. `rowContent` carried a
            // second one (`Tok.rowPaddingV`) from before this card had a border,
            // when a row needed its own breathing room. Inside a bordered card
            // with `Tok.rowSpacing` between cards it was 4pt of nothing.
            .padding(.horizontal, Tok.space3)
            .padding(.vertical, Tok.space2)
            .background(
                RoundedRectangle(cornerRadius: Tok.radiusMedium)
                    .fill(Tok.raised)
            )
            .overlay(
                RoundedRectangle(cornerRadius: Tok.radiusMedium)
                    .strokeBorder(Tok.hairlineStrong, lineWidth: Tok.hairlineWidth)
            )
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
        .disabled(accounts.isPending(account.name))
        if account.health == .needsRelogin {
            Button("Re-login…") { onRelogin() }
        }
        Divider()
        // Hidden entirely while `control` cannot answer the question at all
        // (``ControlAccountController/unavailable``) — an older `tcr` has no
        // `control` subcommand, and an action this build cannot even read back
        // is worse than no action: a click would exit non-zero with a message
        // about a route that does not exist, on every single row, forever.
        if !control.unavailable {
            if control.isControl(account.name) {
                Button("Clear Control Account") {
                    Task { await performSetControl(name: nil, key: account.name) }
                }
                .disabled(control.isPending(account.name))
            } else {
                Button("Use as Control Account") {
                    Task { await performSetControl(name: account.name, key: account.name) }
                }
                .disabled(control.isPending(account.name))
            }
        }
        Divider()
        Button("Copy Account Name") {
            copyToPasteboard(account.name)
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
        .disabled(removeController.isPending(account.name))
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
                Button("Remove from \(group)") {
                    Task { await groupController.remove(account: account.name, from: group) }
                }
                Button("Delete group “\(group)” for everyone…") {
                    confirmDeleteGroup(group)
                }
            case .removeAll:
                Button("Remove from all groups") {
                    Task {
                        for group in (account.groups ?? []) {
                            await groupController.remove(account: account.name, from: group)
                        }
                    }
                }
            case .addToGroup:
                addToGroupMenu
            }
        }
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
                        Task { await groupController.add(account: account.name, to: group) }
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
            Task { await groupController.add(account: account.name, to: name) }
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
        Task { await removeController.remove(account: account.name) }
    }

    /// Shared by "Copy Account Name" and the per-group command copy — one
    /// place that clears then sets, so every copy in this menu behaves the
    /// same way.
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
            .font(Tok.bodyFont)
            .foregroundStyle(.secondary)
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
        let attempt = await accounts.setEnabled(enabling, account: account.name)
        guard case .accepted(let notice) = attempt else { return }
        let readback = await onChanged()
        accounts.record(
            readback: readback,
            requestedEnabled: enabling,
            account: account.name,
            notice: notice
        )
    }

    private var information: some View {
        VStack(alignment: .leading, spacing: Tok.rowLineSpacing) {
            HStack(spacing: Tok.tightSpacing) {
                Text(account.name)
                    .font(Tok.bodyFont)
                    .foregroundStyle(account.disabled ? Tok.disabled : Tok.ink)
                    .lineLimit(1)
                    // Middle truncation eats the middle of an address, which is
                    // exactly the part that distinguishes two accounts on the
                    // same domain. Truncation hides content, so the full value
                    // has to stay reachable somewhere.
                    .truncationMode(.middle)
                    .help(account.name)
                    .textSelection(.enabled)
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
                // The entire group UI now that the dedicated group views are
                // gone (bridge: `docs/plans/group-tags-bridge.md`) — one
                // small colored tag per membership, right on the pills line
                // beside CONTROL/ROTATING/quota so it reads as one more fact
                // about the account. At most two, with the rest collapsed to
                // a `+N` chip: the panel is `Tok.panelWidth` (380pt) wide and
                // an account can be in many groups. An ungrouped account
                // draws nothing here — no chip, no reserved space.
                ForEach(Array(account.groupTags.prefix(2))) { tag in
                    GroupChip(tag: tag)
                }
                if account.groupTags.count > 2 {
                    StatusPill("+\(account.groupTags.count - 2)", tint: Tok.inkFaint)
                        .help(account.groupTags.map(\.name).joined(separator: ", "))
                }
            }
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
                // This 5h window's spend goes here once the proxy puts `usage`
                // on the wire. Empty until then — a placeholder would claim a
                // number the panel does not have.
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
                        .font(Tok.secondaryFont)
                        // `active` and `error` must not be pixel-identical: an
                        // `error` account is what the UNMEASURED pill used to
                        // wear too, and the raw word alone drew in the same
                        // grey as a healthy account right above it.
                        .foregroundStyle(account.health == .needsRelogin ? Tok.spent : .secondary)
                        .lineLimit(1)
                }
                if !countersAreStructural {
                    Text("\(QuotaFormat.count(account.requests)) req · cache \(cacheLabel)")
                        .font(Tok.detailDigitFont)
                        .foregroundStyle(.tertiary)
                        .lineLimit(1)
                }
            }
            if let error = account.lastStreamError, !error.isEmpty {
                // A nil count here is not a quantity with an unknown value —
                // unlike `account.requests` above, this number is a MODIFIER
                // on the error string that is already being displayed, and
                // the error alone is the actionable fact regardless of how
                // many times it happened. See
                // `QuotaFormat.streamErrorLabel(count:error:)`'s doc comment
                // for why this suppresses the multiplier on `nil` instead of
                // following `QuotaFormat.count`'s "n/a" the way the line
                // above does.
                Text(QuotaFormat.streamErrorLabel(count: account.streamErrorCount, error: error))
                    .font(Tok.detailFont)
                    .foregroundStyle(Tok.spent)
                    .lineLimit(2)
            }
            if let failure = accounts.failure(for: account.name) {
                // `tcr`'s own words, verbatim. A toggle that did not happen must
                // never be indistinguishable from one that did.
                Label(failure.summary, systemImage: Tok.unreadableGlyph)
                    .font(Tok.detailFont)
                    .foregroundStyle(Tok.spent)
                    .fixedSize(horizontal: false, vertical: true)
            }
            if let failure = removeController.failure(for: account.name) {
                Label(failure.summary, systemImage: Tok.unreadableGlyph)
                    .font(Tok.detailFont)
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
                .font(Tok.secondaryDigitFont)
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
                .font(Tok.secondaryDigitFont)
                // Demoted alongside the bar, not left `.secondary`: the eye
                // should group these digits as historical rather than live, the
                // same distinction `fiveHourTint` draws for the fill. The number
                // itself is unchanged — it is still true, just no longer
                // reachable.
                .foregroundStyle(hasStaleQuotaReading ? Tok.disabled : .secondary)
            // Drawn whenever the wire has a reset for this window, beside the
            // number it belongs to. Colour: `captionTint(for:)`.
            if let caption = QuotaFormat.resetCaption(resetAtMs: resetAtMs, now: Date()) {
                Text(caption)
                    .font(Tok.secondaryFont)
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
        if removeController.needsRestart(account.name) {
            Label(
                "Removed from config and stopped. Stays listed as disabled until the proxy restarts.",
                systemImage: "arrow.triangle.2.circlepath"
            )
            .font(Tok.detailFont)
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
        if let verdict = accounts.verdict(for: account.name, reportedDisabled: account.disabled) {
            Label(verdict.rowLabel, systemImage: Self.verdictGlyph(verdict))
                .font(Tok.detailFont)
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
            .font(Tok.detailFont)
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
        let pending = accounts.isPending(account.name)
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
        .font(Tok.detailFont)
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

    /// `null` cache ratio means "nothing to divide by" — say so rather than
    /// printing a 0% that reads as a measurement.
    private var cacheLabel: String {
        guard let ratio = account.cacheHitRatio else { return "n/a" }
        return percent(ratio)
    }

    private func percent(_ value: Double) -> String {
        "\(Int((value * 100).rounded()))%"
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
        .frame(width: width, height: Tok.barHeight)
        .animation(reduceMotion ? nil : Tok.standardAnimation, value: fraction)
        .accessibilityElement()
        .accessibilityLabel(label)
        .accessibilityValue(spokenValue)
    }
}
