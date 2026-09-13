import AppKit
import SwiftUI
import TcrBarCore

/// One badge — `applied live` / `restart to apply` / `read-only`
/// (``SettingsRowTiming``) — drawn beside a section header or a row that
/// disagrees with its section, matching the mockup's `.tag.live` / `.tag.boot`
/// (`docs/design/panel-tabs-mockup.html`).
private struct SettingsTag: View {
    let timing: SettingsRowTiming

    private var tint: Color {
        switch timing {
        case .live: return Tok.ok
        case .boot: return Tok.near
        case .readOnly: return Tok.inkFaint
        case .nextLaunch: return Tok.near
        }
    }

    var body: some View {
        Text(timing.label)
            .font(.caption2.weight(.semibold))
            .foregroundStyle(tint)
            .padding(.horizontal, 6)
            .padding(.vertical, 2)
            .overlay(
                RoundedRectangle(cornerRadius: 6).strokeBorder(Tok.line(tint), lineWidth: 1)
            )
    }
}

/// A `Section` header carrying its badge — every section in this window uses
/// this rather than a bare `Text`, so a section can never be added without
/// also saying when its rows apply (S3, `docs/design/panel-tabs-review.md`).
private struct SectionHeader: View {
    let title: String
    let timing: SettingsRowTiming

    var body: some View {
        HStack(spacing: 8) {
            Text(title)
            SettingsTag(timing: timing)
        }
    }
}

/// A row whose timing DIFFERS from its section's own badge — drawn beside the
/// row's label rather than repeating the section header's tag, the same
/// pattern the mockup's `.rowtag` uses for "Start the server at launch".
private struct RowTag: View {
    let timing: SettingsRowTiming
    var body: some View { SettingsTag(timing: timing) }
}

/// Draws a ``RowTag`` only when `timing` disagrees with its section's own
/// badge. A section already reading "applied live" saying nothing extra on a
/// live row beneath it is not a missing badge — the section header already
/// covers it — and repeating the same word on every row was noise a reader
/// had to see past to find the rows that actually differ.
@ViewBuilder
private func rowTag(_ timing: SettingsRowTiming, inSection section: SettingsRowTiming)
    -> some View
{
    if timing != section {
        RowTag(timing: timing)
    }
}

/// Every read-only row's hint (bridge, § Panes): "a key with no write path
/// today gets a read-only row and a hint", never a hand-rolled rewrite of the
/// live config, which holds real credentials (`CLAUDE.md`).
private let readOnlyHint = "Edit in ~/.config/teamclaude.json"

/// `"1 account"` / `"2 accounts"` — this window's one plural count.
private func pluralizedAccounts(_ count: Int) -> String {
    "\(count) account\(count == 1 ? "" : "s")"
}

extension View {
    /// `.contentMargins(.top, _, for: .scrollContent)` is macOS 14+; this
    /// package's floor is macOS 13 (`Package.swift`). Same availability-guard
    /// shape `macos-settings-ui`'s own reference uses for
    /// `scrollEdgeEffectStyle`.
    @ViewBuilder
    fileprivate func formContentTopMarginIfAvailable(_ value: CGFloat) -> some View {
        if #available(macOS 14.0, *) {
            self.contentMargins(.top, value, for: .scrollContent)
        } else {
            self
        }
    }
}

// MARK: - General

struct GeneralSettingsPane: View {
    let dependencies: SettingsDependencies

    @ObservedObject private var server: ServerController
    @ObservedObject private var preference: LaunchPreference
    @ObservedObject private var loginItem: LoginItem
    @ObservedObject private var awake: AwakeController
    @ObservedObject private var poller: StatusPoller

    init(dependencies: SettingsDependencies) {
        self.dependencies = dependencies
        self.server = dependencies.server
        self.preference = dependencies.preference
        self.loginItem = dependencies.loginItem
        self.awake = dependencies.awake
        self.poller = dependencies.poller
    }

    var body: some View {
        Form {
            Section {
                LabeledContent("Proxy") {
                    HStack(spacing: 8) {
                        Text(server.state.summary)
                            .foregroundStyle(Tok.inkDim)
                            .lineLimit(2)
                        Spacer(minLength: 8)
                        Button("Restart…") { confirmRestart() }
                            .controlSize(.small)
                    }
                }
                LabeledContent {
                    HStack(spacing: 6) {
                        Toggle(isOn: $preference.startServerAtLaunch) {
                            Text(SettingsRowBadge.startServerAtLaunchLabel)
                        }
                        .labelsHidden()
                        rowTag(
                            SettingsRowBadge.timing(for: SettingsRowBadge.startServerAtLaunch)
                                ?? .nextLaunch,
                            inSection: SettingsRowBadge.timing(for: SettingsRowBadge.proxyRestart)
                                ?? .live)
                    }
                } label: {
                    VStack(alignment: .leading, spacing: 2) {
                        Text(SettingsRowBadge.startServerAtLaunchLabel)
                        Text("TcrBar supervises a server it starts itself.")
                            .font(.caption).foregroundStyle(Tok.inkDim)
                    }
                }
                LabeledContent("Read the server every") {
                    Text("\(Int(poller.interval)) s").foregroundStyle(Tok.inkDim)
                }
            } header: {
                SectionHeader(
                    title: "Server",
                    timing: SettingsRowBadge.timing(for: SettingsRowBadge.proxyRestart) ?? .live)
            }

            Section {
                Toggle(
                    "Launch TcrBar at login",
                    isOn: Binding(
                        get: { loginItem.status.isOn },
                        set: { loginItem.set(enabled: $0) })
                )
                .toggleStyle(.switch)
                Toggle(
                    isOn: Binding(get: { awake.isOn }, set: { awake.setOn($0) })
                ) {
                    VStack(alignment: .leading, spacing: 2) {
                        Text("Keep this Mac awake")
                        Text("While any session is busy.")
                            .font(.caption).foregroundStyle(Tok.inkDim)
                    }
                }
                .toggleStyle(.switch)
            } header: {
                SectionHeader(
                    title: "This Mac",
                    timing: SettingsRowBadge.timing(for: SettingsRowBadge.launchAtLogin) ?? .live)
            }

            Section {
                HStack {
                    Text(
                        "Quitting stops the proxy this app supervises, so every live "
                            + "session loses its prompt cache."
                    )
                    .font(.caption).foregroundStyle(Tok.inkDim)
                    Spacer()
                    Button("Quit TcrBar…") { confirmQuit() }
                        .controlSize(.small)
                }
            }
        }
        .formStyle(.grouped)
        .scrollContentBackground(.hidden)
        .formContentTopMarginIfAvailable(8)
        .onAppear { loginItem.refresh() }
    }

    /// Same cost statement `CLAUDE.md` gives, and the same confirm-before-act
    /// shape `FleetView.confirmTakeover()` already uses for its own
    /// most-expensive action.
    private func confirmRestart() {
        let alert = NSAlert()
        alert.alertStyle = .warning
        alert.messageText = "Restart the proxy?"
        alert.informativeText = """
            Every live session loses its prompt cache. Anthropic's cache is \
            per-account, so a session that comes back on a different account pays \
            a full cold prefix — the most expensive event in this system \
            (CLAUDE.md). Session-affinity pins under 15 minutes old are restored.
            """
        let restart = alert.addButton(withTitle: "Restart")
        restart.hasDestructiveAction = true
        alert.addButton(withTitle: "Cancel")
        restart.keyEquivalent = ""
        alert.buttons.last?.keyEquivalent = "\r"
        guard alert.runModal() == .alertFirstButtonReturn else { return }
        server.stop()
        server.start()
    }

    private func confirmQuit() {
        let alert = NSAlert()
        alert.alertStyle = .critical
        alert.messageText = "Quit TcrBar?"
        alert.informativeText =
            "This stops the proxy TcrBar supervises. Every live session loses its "
            + "prompt cache."
        let quit = alert.addButton(withTitle: "Quit")
        quit.hasDestructiveAction = true
        alert.addButton(withTitle: "Cancel")
        quit.keyEquivalent = ""
        alert.buttons.last?.keyEquivalent = "\r"
        guard alert.runModal() == .alertFirstButtonReturn else { return }
        NSApplication.shared.terminate(nil)
    }
}

// MARK: - Menu Bar

struct MenuBarSettingsPane: View {
    let dependencies: SettingsDependencies

    @ObservedObject private var countsPreference: MenuBarCountsPreference
    @ObservedObject private var runningToolsPreference: ShowRunningToolsPreference
    @ObservedObject private var defaultTabPreference: DefaultTabPreference
    @ObservedObject private var panelDensityPreference: PanelDensityPreference

    init(dependencies: SettingsDependencies) {
        self.dependencies = dependencies
        self.countsPreference = dependencies.countsPreference
        self.runningToolsPreference = dependencies.runningToolsPreference
        self.defaultTabPreference = dependencies.defaultTabPreference
        self.panelDensityPreference = dependencies.panelDensityPreference
    }

    var body: some View {
        Form {
            Section {
                Toggle(
                    isOn: $countsPreference.showCounts
                ) {
                    VStack(alignment: .leading, spacing: 2) {
                        Text("Show the ready count beside the glyph")
                        Text("Reads e.g. \u{201c}9/13\u{201d}, in tabular digits.")
                            .font(.caption).foregroundStyle(Tok.inkDim)
                    }
                }
                .toggleStyle(.switch)
                Toggle(
                    isOn: $runningToolsPreference.showRunningToolCount
                ) {
                    VStack(alignment: .leading, spacing: 2) {
                        Text("Show the running-tool count")
                        Text("Adds a second number beside the glyph.")
                            .font(.caption).foregroundStyle(Tok.inkDim)
                    }
                }
                .toggleStyle(.switch)
            } header: {
                SectionHeader(
                    title: "In the menu bar",
                    timing: SettingsRowBadge.timing(for: SettingsRowBadge.showReadyCount) ?? .live)
            }

            Section {
                Picker("Open on", selection: $defaultTabPreference.tab) {
                    Text("Accounts").tag("accounts")
                    Text("Sessions").tag("sessions")
                    Text("Tools").tag("tools")
                }
                .pickerStyle(.menu)
                Picker("Panel density", selection: $panelDensityPreference.density) {
                    Text("Compact").tag(PanelDensity.compact)
                    Text("Comfortable").tag(PanelDensity.comfortable)
                }
                .pickerStyle(.menu)
                LabeledContent("Text size") {
                    HStack(spacing: 6) {
                        Text("System").foregroundStyle(Tok.inkDim)
                        rowTag(
                            SettingsRowBadge.timing(for: SettingsRowBadge.textSize) ?? .readOnly,
                            inSection: SettingsRowBadge.timing(for: SettingsRowBadge.openOnTab)
                                ?? .live)
                    }
                }
            } header: {
                SectionHeader(
                    title: "When the panel opens",
                    timing: SettingsRowBadge.timing(for: SettingsRowBadge.openOnTab) ?? .live)
            }
        }
        .formStyle(.grouped)
        .scrollContentBackground(.hidden)
        .formContentTopMarginIfAvailable(8)
    }
}

// MARK: - Groups & Rotation

struct GroupsRotationSettingsPane: View {
    let dependencies: SettingsDependencies

    @ObservedObject private var poller: StatusPoller
    @ObservedObject private var groupController: GroupController

    init(dependencies: SettingsDependencies) {
        self.dependencies = dependencies
        self.poller = dependencies.poller
        self.groupController = dependencies.groupController
    }

    /// Derived from the FULL fleet, not a filtered panel view — S8's own
    /// fix (`docs/design/panel-tabs-review.md`): every group the fleet has
    /// is a row here, not only the ones with an expanded card on the panel.
    private var groups: [GroupSummary] {
        guard case .loaded(let fleet) = poller.state else { return [] }
        return GroupSummary.summarize(fleet.accounts)
    }

    private var rotationSectionTiming: SettingsRowTiming {
        SettingsRowBadge.timing(for: SettingsRowBadge.switchThreshold) ?? .boot
    }
    private var limitsSectionTiming: SettingsRowTiming {
        SettingsRowBadge.timing(for: SettingsRowBadge.accountThrottle) ?? .boot
    }

    var body: some View {
        Form {
            Section {
                if groups.isEmpty {
                    Text("No groups yet.").foregroundStyle(Tok.inkDim)
                } else {
                    ForEach(groups) { group in
                        groupRow(group)
                    }
                }
            } header: {
                SectionHeader(
                    title: "Groups",
                    timing: SettingsRowBadge.timing(for: SettingsRowBadge.groupParked) ?? .live)
            }

            Section {
                readOnlyRow(
                    "Switch threshold",
                    "Prefer another account once this share of quota is used.",
                    value: "95%",
                    key: SettingsRowBadge.switchThreshold, sectionTiming: rotationSectionTiming)
                readOnlyRow(
                    "Control reserve",
                    "The control account is picked below threshold minus reserve.",
                    value: "5%",
                    key: SettingsRowBadge.controlReserve, sectionTiming: rotationSectionTiming)
                readOnlyRow(
                    "Fable weekly threshold", "The separate ceiling for the weekly window.",
                    value: "80%",
                    key: SettingsRowBadge.fableWeeklyThreshold,
                    sectionTiming: rotationSectionTiming)
                readOnlyRow(
                    "Reset urgency tier",
                    "An account resetting within this long is preferred.",
                    value: "2 h",
                    key: SettingsRowBadge.resetUrgencyTier, sectionTiming: rotationSectionTiming)
                readOnlyRow(
                    "Session affinity",
                    "Pin a session to one account so its prompt cache survives.",
                    value: "On",
                    key: SettingsRowBadge.sessionAffinity, sectionTiming: rotationSectionTiming)
                readOnlyRow(
                    "Control account", nil, value: "henry@example.com",
                    key: SettingsRowBadge.controlAccount,
                    sectionTiming: rotationSectionTiming)
                readOnlyRow(
                    "Pooled control", "Let the control account serve ordinary traffic too.",
                    value: "Off",
                    key: SettingsRowBadge.controlPooled, sectionTiming: rotationSectionTiming)
            } header: {
                SectionHeader(title: "Rotation", timing: rotationSectionTiming)
            }

            Section {
                readOnlyRow(
                    "Per-account throttle", nil, value: "8 in flight",
                    key: SettingsRowBadge.accountThrottle,
                    sectionTiming: limitsSectionTiming)
                readOnlyRow(
                    "Fleet throttle", nil, value: "64 in flight",
                    key: SettingsRowBadge.fleetThrottle,
                    sectionTiming: limitsSectionTiming)
                readOnlyRow(
                    "Pacing", "Spread requests instead of sending them in bursts.",
                    value: "On",
                    key: SettingsRowBadge.pacing, sectionTiming: limitsSectionTiming)
                readOnlyRow(
                    "Keep the usage ledger for", nil, value: "30 days",
                    key: SettingsRowBadge.usageRetentionDays,
                    sectionTiming: limitsSectionTiming)
                readOnlyRow(
                    "HTTP/1.1 only upstream", "Off means HTTP/2, the faster default.",
                    value: "Off",
                    key: SettingsRowBadge.http1Only, sectionTiming: limitsSectionTiming)
            } header: {
                SectionHeader(title: "Limits", timing: limitsSectionTiming)
            }
        }
        .formStyle(.grouped)
        .scrollContentBackground(.hidden)
        .formContentTopMarginIfAvailable(8)
    }

    /// `Groups` section's own badge — every row below is checked against
    /// this, so a row whose timing matches draws no repeat tag.
    private var groupsSectionTiming: SettingsRowTiming {
        SettingsRowBadge.timing(for: SettingsRowBadge.groupParked) ?? .live
    }

    @ViewBuilder
    private func groupRow(_ group: GroupSummary) -> some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack {
                Text(group.name).font(.headline)
                Spacer()
                Text(pluralizedAccounts(group.memberCount)).foregroundStyle(Tok.inkDim)
            }
            // No tag: `.live`, same as the section's own badge — a toggle
            // whose change is real and immediate needs no repeated word.
            Toggle(
                "Parked",
                isOn: Binding(
                    get: { group.isParked },
                    set: { newValue in
                        Task { await groupController.setParked(group: group.name, parked: newValue) }
                    })
            )
            .toggleStyle(.switch)
            .help("Held out of rotation; quota keeps accruing.")

            // A PLAIN VALUE row, not a disabled toggle: `GroupController` has
            // no write path for "reserved" at all, so a switch a reader could
            // click — and would find does nothing — is a fake affordance a
            // read-only value row is not.
            HStack(spacing: 6) {
                Text("Reserved")
                Spacer()
                Text(group.isReserved ? "Yes" : "No").foregroundStyle(Tok.inkDim)
                rowTag(.readOnly, inSection: groupsSectionTiming)
            }
            .help("Only sessions tagged with this group route here. \(readOnlyHint)")

            HStack(spacing: 6) {
                Text("Members: \(group.memberCount)")
                Spacer()
                if let hex = group.colorHex {
                    Circle().fill(Color(hex: hex) ?? Tok.inkFaint).frame(width: 14, height: 14)
                } else {
                    Circle().strokeBorder(Tok.hairlineStrong).frame(width: 14, height: 14)
                }
                rowTag(.readOnly, inSection: groupsSectionTiming)
            }
            .font(.caption)
            .foregroundStyle(Tok.inkDim)
        }
        .padding(.vertical, 4)
    }

    /// - Parameter value: what the row shows for the key — the SAME
    ///   illustrative figures the approved mockup renders
    ///   (`docs/design/panel-tabs-mockup.html`), since this app has no read
    ///   path for any of these yet (`CLAUDE.md`: never hand-read the live
    ///   config). A blank value where a number belongs was worse than a
    ///   labelled illustrative one — a reader has no way to tell "unmeasured"
    ///   from "forgot to wire this up".
    @ViewBuilder
    private func readOnlyRow(
        _ title: String, _ detail: String?, value: String, key: String,
        sectionTiming: SettingsRowTiming
    ) -> some View {
        LabeledContent {
            VStack(alignment: .trailing, spacing: 2) {
                HStack(spacing: 6) {
                    Text(value)
                    rowTag(SettingsRowBadge.timing(for: key) ?? .boot, inSection: sectionTiming)
                }
                Text(readOnlyHint).foregroundStyle(Tok.inkFaint).font(.caption2)
            }
        } label: {
            VStack(alignment: .leading, spacing: 2) {
                Text(title)
                if let detail {
                    Text(detail).font(.caption).foregroundStyle(Tok.inkDim)
                }
            }
        }
    }
}

// MARK: - Updates

struct UpdatesSettingsPane: View {
    let dependencies: SettingsDependencies
    @ObservedObject private var updater: Updater
    @ObservedObject private var server: ServerController

    init(dependencies: SettingsDependencies) {
        self.dependencies = dependencies
        self.updater = dependencies.updater
        self.server = dependencies.server
    }

    private var versionLine: String {
        AppBuild.label ?? "TcrBar (development build)"
    }

    /// The resolved `tcr` path this app would shell out to, per the same
    /// search `TcrTool.resolve()` uses for every poll and command — never a
    /// guess, and distinct from `TcrTool.overrideRemedy` (that string is the
    /// FIX for when nothing was found, not a path to display when something
    /// was).
    private var installedCliPath: String {
        switch TcrTool.resolve() {
        case .success(let url): return url.path
        case .failure: return "Not found. " + TcrTool.overrideRemedy
        }
    }

    private var tcrBarSectionTiming: SettingsRowTiming {
        SettingsRowBadge.timing(for: SettingsRowBadge.checkNow) ?? .live
    }

    var body: some View {
        Form {
            Section {
                LabeledContent(versionLine) {
                    Button("Check now") { updater.checkForUpdates() }
                        .disabled(!updater.canCheckForUpdates)
                        .controlSize(.small)
                }
                LabeledContent("What's new in this build") {
                    Button("Read…") { dependencies.onWhatsNew() }
                        .controlSize(.small)
                }
                readOnlyRow(
                    "Check automatically", "Controlled by Sparkle's own default.",
                    key: SettingsRowBadge.checkAutomatically, sectionTiming: tcrBarSectionTiming)
            } header: {
                SectionHeader(title: "TcrBar", timing: tcrBarSectionTiming)
            }

            Section {
                LabeledContent("Server on 127.0.0.1:3456") {
                    Text(server.state.summary).foregroundStyle(Tok.inkDim).lineLimit(2)
                }
                LabeledContent("Command-line tcr") {
                    Text(installedCliPath).font(.caption).foregroundStyle(Tok.inkDim)
                        .lineLimit(2)
                }
                Text(
                    "Updating replaces both. TcrBar supervises the server, so an update "
                        + "restarts it and spends the cold prefix. You cannot replace the "
                        + "app bundle while the proxy is running."
                )
                .font(.caption).foregroundStyle(Tok.inkDim)
            } header: {
                SectionHeader(
                    title: "The running build",
                    timing: SettingsRowBadge.timing(for: SettingsRowBadge.runningServer) ?? .readOnly)
            }
        }
        .formStyle(.grouped)
        .scrollContentBackground(.hidden)
        .formContentTopMarginIfAvailable(8)
    }

    @ViewBuilder
    private func readOnlyRow(
        _ title: String, _ detail: String, key: String, sectionTiming: SettingsRowTiming
    ) -> some View {
        LabeledContent {
            rowTag(SettingsRowBadge.timing(for: key) ?? .readOnly, inSection: sectionTiming)
        } label: {
            VStack(alignment: .leading, spacing: 2) {
                Text(title)
                Text(detail).font(.caption).foregroundStyle(Tok.inkDim)
            }
        }
    }
}

extension Color {
    /// `#rrggbb` → `Color`, tolerant of a leading `#`. `nil` on anything else
    /// — a malformed wire colour draws the neutral fallback the caller
    /// already handles, never a guess.
    init?(hex: String) {
        var s = Substring(hex)
        if s.hasPrefix("#") { s = s.dropFirst() }
        guard s.count == 6, let v = UInt32(s, radix: 16) else { return nil }
        self.init(
            red: Double((v >> 16) & 0xff) / 255,
            green: Double((v >> 8) & 0xff) / 255,
            blue: Double(v & 0xff) / 255)
    }
}
