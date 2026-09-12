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

/// Every read-only row's hint (bridge, § Panes): "a key with no write path
/// today gets a read-only row and a hint", never a hand-rolled rewrite of the
/// live config, which holds real credentials (`CLAUDE.md`).
private let readOnlyHint = "Edit in ~/.config/teamclaude.json"

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
                            .foregroundStyle(.secondary)
                            .lineLimit(2)
                        Spacer(minLength: 8)
                        Button("Restart…") { confirmRestart() }
                            .controlSize(.small)
                    }
                }
                LabeledContent {
                    HStack(spacing: 6) {
                        Toggle("", isOn: $preference.startServerAtLaunch)
                            .labelsHidden()
                        RowTag(timing: SettingsRowBadge.timing(for: SettingsRowBadge.startServerAtLaunch) ?? .boot)
                    }
                } label: {
                    VStack(alignment: .leading, spacing: 2) {
                        Text("Start the server at launch")
                        Text("TcrBar supervises a server it starts itself.")
                            .font(.caption).foregroundStyle(.secondary)
                    }
                }
                LabeledContent("Read the server every") {
                    Text("\(Int(poller.interval)) s").foregroundStyle(.secondary)
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
                            .font(.caption).foregroundStyle(.secondary)
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
                    .font(.caption).foregroundStyle(.secondary)
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

    init(dependencies: SettingsDependencies) {
        self.dependencies = dependencies
        self.countsPreference = dependencies.countsPreference
        self.runningToolsPreference = dependencies.runningToolsPreference
        self.defaultTabPreference = dependencies.defaultTabPreference
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
                            .font(.caption).foregroundStyle(.secondary)
                    }
                }
                .toggleStyle(.switch)
                Toggle(
                    isOn: $runningToolsPreference.showRunningToolCount
                ) {
                    VStack(alignment: .leading, spacing: 2) {
                        Text("Show the running-tool count")
                        Text("Adds a second number beside the glyph.")
                            .font(.caption).foregroundStyle(.secondary)
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
                LabeledContent("Text size") {
                    Text("System").foregroundStyle(.secondary)
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

    var body: some View {
        Form {
            Section {
                if groups.isEmpty {
                    Text("No groups yet.").foregroundStyle(.secondary)
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
                    key: SettingsRowBadge.switchThreshold)
                readOnlyRow(
                    "Control reserve",
                    "The control account is picked below threshold minus reserve.",
                    key: SettingsRowBadge.controlReserve)
                readOnlyRow(
                    "Fable weekly threshold", "The separate ceiling for the weekly window.",
                    key: SettingsRowBadge.fableWeeklyThreshold)
                readOnlyRow(
                    "Reset urgency tier",
                    "An account resetting within this long is preferred.",
                    key: SettingsRowBadge.resetUrgencyTier)
                readOnlyRow(
                    "Session affinity",
                    "Pin a session to one account so its prompt cache survives.",
                    key: SettingsRowBadge.sessionAffinity)
                readOnlyRow("Control account", nil, key: SettingsRowBadge.controlAccount)
                readOnlyRow(
                    "Pooled control", "Let the control account serve ordinary traffic too.",
                    key: SettingsRowBadge.controlPooled)
            } header: {
                SectionHeader(
                    title: "Rotation",
                    timing: SettingsRowBadge.timing(for: SettingsRowBadge.switchThreshold) ?? .boot)
            }

            Section {
                readOnlyRow("Per-account throttle", nil, key: SettingsRowBadge.accountThrottle)
                readOnlyRow("Fleet throttle", nil, key: SettingsRowBadge.fleetThrottle)
                readOnlyRow(
                    "Pacing", "Spread requests instead of sending them in bursts.",
                    key: SettingsRowBadge.pacing)
                readOnlyRow(
                    "Keep the usage ledger for", nil, key: SettingsRowBadge.usageRetentionDays)
                readOnlyRow(
                    "HTTP/1.1 only upstream", "Off means HTTP/2, the faster default.",
                    key: SettingsRowBadge.http1Only)
            } header: {
                SectionHeader(
                    title: "Limits",
                    timing: SettingsRowBadge.timing(for: SettingsRowBadge.accountThrottle) ?? .boot)
            }
        }
        .formStyle(.grouped)
        .scrollContentBackground(.hidden)
        .formContentTopMarginIfAvailable(8)
    }

    @ViewBuilder
    private func groupRow(_ group: GroupSummary) -> some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack {
                Text(group.name).font(.headline)
                Spacer()
                Text("\(group.memberCount) accounts").foregroundStyle(.secondary)
            }
            HStack(spacing: 6) {
                Toggle(
                    "Parked",
                    isOn: Binding(
                        get: { group.isParked },
                        set: { newValue in
                            Task { await groupController.setParked(group: group.name, parked: newValue) }
                        })
                )
                .toggleStyle(.switch)
                RowTag(timing: SettingsRowBadge.timing(for: SettingsRowBadge.groupParked) ?? .live)
            }
            .help("Held out of rotation; quota keeps accruing.")

            HStack(spacing: 6) {
                Toggle("Reserved", isOn: .constant(group.isReserved))
                    .toggleStyle(.switch)
                    .disabled(true)
                RowTag(timing: .readOnly)
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
                RowTag(timing: .readOnly)
            }
            .font(.caption)
            .foregroundStyle(.secondary)
        }
        .padding(.vertical, 4)
    }

    @ViewBuilder
    private func readOnlyRow(_ title: String, _ detail: String?, key: String) -> some View {
        LabeledContent {
            HStack(spacing: 6) {
                Text(readOnlyHint).foregroundStyle(.secondary).font(.caption)
                RowTag(timing: SettingsRowBadge.timing(for: key) ?? .boot)
            }
        } label: {
            VStack(alignment: .leading, spacing: 2) {
                Text(title)
                if let detail {
                    Text(detail).font(.caption).foregroundStyle(.secondary)
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
                    key: SettingsRowBadge.checkAutomatically)
            } header: {
                SectionHeader(
                    title: "TcrBar",
                    timing: SettingsRowBadge.timing(for: SettingsRowBadge.checkNow) ?? .live)
            }

            Section {
                LabeledContent("Server on 127.0.0.1:3456") {
                    Text(server.state.summary).foregroundStyle(.secondary).lineLimit(2)
                }
                LabeledContent("Command-line tcr") {
                    Text(installedCliPath).font(.caption).foregroundStyle(.secondary)
                        .lineLimit(2)
                }
                Text(
                    "Updating replaces both. TcrBar supervises the server, so an update "
                        + "restarts it and spends the cold prefix. You cannot replace the "
                        + "app bundle while the proxy is running."
                )
                .font(.caption).foregroundStyle(.secondary)
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
    private func readOnlyRow(_ title: String, _ detail: String, key: String) -> some View {
        LabeledContent {
            RowTag(timing: SettingsRowBadge.timing(for: key) ?? .readOnly)
        } label: {
            VStack(alignment: .leading, spacing: 2) {
                Text(title)
                Text(detail).font(.caption).foregroundStyle(.secondary)
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
