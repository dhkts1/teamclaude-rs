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
    /// Owned by the app so it survives the panel closing; bound here so the
    /// checkbox and the launch path can never disagree about its value.
    @Binding var startServerAtLaunch: Bool

    /// Measured height of the account rows.
    ///
    /// A `ScrollView` has a flexible ideal height, and the `MenuBarExtra` window
    /// sizes itself to its content's *ideal* height — so a scroll view carrying
    /// only a `maxHeight` collapses to roughly one row no matter how many
    /// accounts the fleet has. Measuring the rows and giving the scroll view a
    /// concrete height is what makes the panel grow with the fleet.
    @State private var rowsHeight: CGFloat = 0

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
            Text(poller.state.summary)
                .font(Tok.secondaryFont)
                .foregroundStyle(poller.state.isHealthyRead ? .secondary : .primary)
                .fixedSize(horizontal: false, vertical: true)
            if case .loaded(let fleet) = poller.state, !fleet.accounts.isEmpty {
                capacitySummary(fleet)
            }
            if case .loaded(let fleet) = poller.state, let sha = fleet.serverSha {
                Text("server \(sha)\(fleet.serverDirty ? "-dirty" : "")")
                    .font(Tok.detailDigitFont)
                    .foregroundStyle(.tertiary)
            }
        }
    }

    /// The one question the panel exists to answer: is there capacity right now,
    /// and if not, when does it come back. All counting lives on `Fleet`.
    private func capacitySummary(_ fleet: Fleet) -> some View {
        VStack(alignment: .leading, spacing: Tok.tightSpacing) {
            Text(fleet.capacitySummary)
                .font(.subheadline.weight(.semibold))
                .foregroundStyle(Tok.color(for: fleet.capacityState))
                .fixedSize(horizontal: false, vertical: true)
            HStack(spacing: Tok.tightSpacing) {
                ForEach(Array(fleet.breakdown.enumerated()), id: \.offset) { index, tally in
                    if index > 0 {
                        Text("·").font(Tok.secondaryFont).foregroundStyle(.tertiary)
                    }
                    Text(tally.label)
                        .font(Tok.secondaryDigitFont)
                        .foregroundStyle(Tok.color(for: tally.kind))
                }
            }
            .accessibilityElement(children: .ignore)
            .accessibilityLabel(fleet.breakdownLabel)
        }
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
                    + "`defaults write com.github.dhkts1.tcrbar \(TcrTool.overrideDefaultsKey) <path>`.",
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
            ScrollView {
                VStack(alignment: .leading, spacing: Tok.rowSpacing) {
                    ForEach(fleet.rowsInDisplayOrder) { account in
                        AccountRow(
                            account: account,
                            countersAreStructural: fleet.source.countersAreStructural,
                            accounts: accounts,
                            onChanged: { await poller.pollOnce() }
                        )
                    }
                }
                .background(
                    GeometryReader { proxy in
                        Color.clear.preference(key: RowsHeightKey.self, value: proxy.size.height)
                    }
                )
            }
            .frame(height: min(max(rowsHeight, Tok.rowSpacing), Tok.panelMaxHeight))
            .onPreferenceChange(RowsHeightKey.self) { rowsHeight = $0 }
        }
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
            Text(server.state.summary)
                .font(Tok.secondaryFont)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
            HStack {
                if server.state.isOurChild {
                    Button("Stop server") { server.stop() }
                } else {
                    Button("Start server") { server.start() }
                }
                Button("Refresh") { Task { await poller.pollOnce() } }
                Spacer()
                Button("Quit") { NSApplication.shared.terminate(nil) }
            }
            .buttonStyle(.bordered)

            launchAtLogin
            startServerToggle

            dangerZone
        }
    }

    /// Bring the proxy up when TcrBar starts. Pairs with "Launch at login" to
    /// mean "the proxy is always up".
    ///
    /// The warning is not decoration. Once TcrBar supervises the server, Quit
    /// stops it — correct for a supervisor, and a genuinely expensive surprise if
    /// nobody said so before the box was ticked.
    private var startServerToggle: some View {
        VStack(alignment: .leading, spacing: Tok.tightSpacing) {
            Toggle("Start server at launch", isOn: $startServerAtLaunch)
                .toggleStyle(.checkbox)
                .font(Tok.secondaryFont)
                .help(
                    "Runs `tcr server --no-replace` when TcrBar starts, so a proxy "
                        + "that is already serving is never disturbed."
                )
            if startServerAtLaunch {
                Text("TcrBar supervises the server, so quitting TcrBar stops it.")
                    .font(Tok.detailFont)
                    .foregroundStyle(Tok.near)
                    .fixedSize(horizontal: false, vertical: true)
            }
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
    /// this call `startTakingOverPort()`, which spawns `tcr server` without
    /// `--no-replace` — the replacement is performed by `tcr`'s own singleton.
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

/// Carries the measured height of the account rows out of the scroll view.
private struct RowsHeightKey: PreferenceKey {
    static let defaultValue: CGFloat = 0
    static func reduce(value: inout CGFloat, nextValue: () -> CGFloat) {
        value = max(value, nextValue())
    }
}

/// One account.
///
/// The enable/disable control is a `tcr` subprocess and nothing else: this app
/// never touches `~/.config/teamclaude.json`, which holds credentials. The row
/// does not optimistically flip its own `disabled` — on success it asks for a
/// fresh poll and renders whatever `tcr status` then reports, so a call that
/// matched the wrong account (the query is a substring match) shows up as the
/// wrong row changing rather than as a lie. A non-zero exit is rendered in place.
///
/// One thing this row deliberately does not claim: that the *running proxy* has
/// picked the change up. `tcr` rewrites the config; whether a live server re-reads
/// it without a restart is unverified from here, in either direction.
///
/// No confirmation dialog: disabling is reversible and costs nothing.
struct AccountRow: View {
    let account: Account
    let countersAreStructural: Bool
    @ObservedObject var accounts: AccountController
    /// Called after a successful toggle so the row re-reads reality.
    let onChanged: () async -> Void

    /// The single tint for this row's quota evidence. The pill and the bar both
    /// read it, so the two can never disagree about whether a quota is known.
    ///
    /// `Tok.unmeasured`, not `Tok.unknown`: `FleetTally.Kind` documents these as
    /// deliberately separate — "a quota state this build cannot name" is not the
    /// same fact as "no quota state at all", and colouring them alike re-merges
    /// exactly what the optional model exists to keep apart.
    private var quotaTint: Color {
        account.hasQuotaEvidence ? Tok.color(for: account.quotaState) : Tok.unmeasured
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
        if account.disabled { parts.append("disabled") }
        parts.append(
            account.hasQuotaEvidence
                ? "\(account.quotaState.token), \(QuotaFormat.percent(account.quota)) used"
                : "never probed, quota unknown"
        )
        if let hold = account.soonestHold { parts.append(hold.countdownLabel) }
        if let failure = accounts.failure(for: account.name) {
            parts.append("last action failed: \(failure.summary)")
        }
        return parts.joined(separator: ", ")
    }

    var body: some View {
        HStack(alignment: .top, spacing: Tok.tightSpacing) {
            information
                .accessibilityElement(children: .combine)
                .accessibilityLabel(rowAccessibilityLabel)
            toggleButton
        }
        .padding(.vertical, Tok.rowPaddingV)
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
                Spacer(minLength: Tok.tightSpacing)
                if account.disabled { StatusPill("disabled", tint: Tok.disabled) }
                // A never-probed account's `quotaState` is Rust's default, not a
                // reading. Printing `ok` on it would be the panel asserting
                // something nothing has ever checked.
                if account.hasQuotaEvidence {
                    StatusPill(account.quotaState.token, tint: quotaTint)
                } else {
                    StatusPill("unmeasured", tint: quotaTint)
                        .help("Never probed — this account's quota is unknown, not zero.")
                }
            }
            QuotaBar(fraction: account.quota, tint: quotaTint)
            HStack(spacing: Tok.tightSpacing) {
                Text(QuotaFormat.percent(account.quota))
                    .font(Tok.secondaryDigitFont)
                    .foregroundStyle(.secondary)
                Text(
                    "· 5h \(QuotaFormat.percent(account.fiveHour)) "
                        + "· 7d \(QuotaFormat.percent(account.sevenDay))"
                )
                .font(Tok.secondaryDigitFont)
                .foregroundStyle(.secondary)
                Spacer()
                Text(account.status)
                    .font(Tok.secondaryFont)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
            }
            if let hold = account.soonestHold {
                Label(hold.countdownLabel, systemImage: "hourglass")
                    .font(Tok.secondaryFont)
                    .foregroundStyle(Tok.near)
            }
            if !countersAreStructural {
                Text("\(account.requests) req · cache \(cacheLabel)")
                    .font(Tok.detailDigitFont)
                    .foregroundStyle(.tertiary)
            }
            if let error = account.lastStreamError, !error.isEmpty {
                Text("\(account.streamErrorCount)× \(error)")
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
        }
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
            Task {
                if await accounts.setEnabled(enabling, account: account.name) {
                    await onChanged()
                }
            }
        }
        .buttonStyle(.borderless)
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
/// is pixel-identical to `0`, so drawing one would make the panel assert
/// "exhausted" about an account nothing has ever measured. Keeping unknown
/// distinct from empty at the last rendering step is the whole point of the
/// optional model — collapsing it here would undo it after the decoder,
/// `readyCount` and the pill all took care to preserve it.
struct QuotaBar: View {
    let fraction: Double?
    let tint: Color

    /// Honour the system Reduce Motion setting. SwiftUI does not suppress an
    /// explicit `.animation` for you.
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    /// What assistive technology hears.
    ///
    /// Without this the bar is a nameless graphic, so the panel's single most
    /// important number is invisible to VoiceOver — and worse, the nil-vs-zero
    /// distinction that the decoder, `readyCount` and the pill all take care to
    /// preserve would survive only as a dashed outline. "Never measured" and
    /// "exhausted" would be identical again, which is the exact defect the
    /// optional model exists to prevent.
    private var spokenValue: String {
        guard let fraction else { return "not measured" }
        return "\(Int((fraction * 100).rounded())) percent used"
    }

    var body: some View {
        GeometryReader { geo in
            ZStack(alignment: .leading) {
                RoundedRectangle(cornerRadius: Tok.barRadius).fill(Tok.track)
                if let fraction {
                    RoundedRectangle(cornerRadius: Tok.barRadius)
                        .fill(tint)
                        .frame(width: geo.size.width * min(max(fraction, 0), 1))
                } else {
                    RoundedRectangle(cornerRadius: Tok.barRadius)
                        .strokeBorder(tint, style: StrokeStyle(lineWidth: 1, dash: [2, 2]))
                }
            }
        }
        .frame(height: Tok.barHeight)
        .animation(reduceMotion ? nil : Tok.standardAnimation, value: fraction)
        .accessibilityElement()
        .accessibilityLabel("Quota")
        .accessibilityValue(spokenValue)
    }
}
