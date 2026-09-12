import Foundation

/// When a Settings-window row's value takes effect, and the one table every
/// row is checked against (`data/plans/settings-window-bridge.md`, gate 1).
///
/// This exists because of a finding against the earlier mockup
/// (`docs/design/panel-tabs-review.md` § Settings window, S3): 16 of 31 rows
/// carried no marking at all, including every `Limits` row sitting directly
/// under the one section that WAS marked — so silence read as "live". A row
/// with no entry here is exactly that trap, so ``SettingsRowBadgeTests``
/// enumerates every row this window draws and asserts each one has an entry.
///
/// `.readOnly` is a third case, not a variant of `.boot`: a value TcrBar only
/// displays (the running server's sha, the installed `tcr` version) has no
/// "apply" moment at all — there is nothing to restart into.
public enum SettingsRowTiming: Equatable, Sendable {
    /// Read and written by TcrBar itself, or applied live by the proxy
    /// (`Manager::reload_groups_if_changed`, `docs/configuration.md`). The
    /// view reflects the new value on its next poll or preference read.
    case live
    /// Read once by the proxy at start-up (`docs/configuration.md`: "every
    /// other config field is a boot-time snapshot"). A change here is real —
    /// `tcr` writes it — but nothing observes it until the next restart.
    case boot
    /// TcrBar only displays this; there is no write path through this window
    /// at all.
    case readOnly

    /// The two-word tag the mockup and this window both draw
    /// (`docs/design/panel-tabs-mockup.html` `.tag.live` / `.tag.boot`).
    public var label: String {
        switch self {
        case .live: return "applied live"
        case .boot: return "restart to apply"
        case .readOnly: return "read-only"
        }
    }
}

/// The full row registry, keyed by the same short id each pane uses to look
/// itself up. One flat table rather than one enum per pane, so a row that
/// moves pane cannot silently lose its entry — a missing key fails the same
/// way `SettingsRowBadgeTests` checks for.
public enum SettingsRowBadge {
    /// `Server` section, General pane.
    public static let proxyRestart = "server.proxyRestart"
    public static let startServerAtLaunch = "server.startServerAtLaunch"
    public static let pollInterval = "server.pollInterval"
    /// `This Mac` section, General pane.
    public static let launchAtLogin = "mac.launchAtLogin"
    public static let keepAwake = "mac.keepAwake"
    /// `In the menu bar` section, Menu Bar pane.
    public static let showReadyCount = "menuBar.showReadyCount"
    public static let showRunningToolCount = "menuBar.showRunningToolCount"
    /// `When the panel opens` section, Menu Bar pane.
    public static let openOnTab = "menuBar.openOnTab"
    public static let textSize = "menuBar.textSize"
    /// `Groups` section, Groups & Rotation pane — one entry covers every
    /// group row, since they are all the same shape.
    public static let groupParked = "groups.parked"
    public static let groupReserved = "groups.reserved"
    public static let groupMayServeAsControl = "groups.mayServeAsControl"
    public static let groupColor = "groups.color"
    public static let groupMembers = "groups.members"
    /// `Rotation` section — every key here is read once at boot
    /// (`docs/configuration.md`).
    public static let switchThreshold = "rotation.switchThreshold"
    public static let controlReserve = "rotation.controlReserve"
    public static let fableWeeklyThreshold = "rotation.fableWeeklyThreshold"
    public static let resetUrgencyTier = "rotation.resetUrgencyTier"
    public static let sessionAffinity = "rotation.sessionAffinity"
    public static let controlAccount = "rotation.controlAccount"
    public static let controlPooled = "rotation.controlPooled"
    /// `Limits` section — also boot-time.
    public static let accountThrottle = "limits.accountThrottle"
    public static let fleetThrottle = "limits.fleetThrottle"
    public static let pacing = "limits.pacing"
    public static let usageRetentionDays = "limits.usageRetentionDays"
    public static let http1Only = "limits.http1Only"
    /// `TcrBar` section, Updates pane.
    public static let checkNow = "updates.checkNow"
    public static let whatsNew = "updates.whatsNew"
    public static let checkAutomatically = "updates.checkAutomatically"
    /// `The running build` section, Updates pane.
    public static let runningServer = "updates.runningServer"
    public static let installedCli = "updates.installedCli"
    public static let updatingReplacesBoth = "updates.updatingReplacesBoth"

    /// The table itself. Every key above must appear here exactly once —
    /// ``SettingsRowBadgeTests`` enumerates both directions.
    public static let timing: [String: SettingsRowTiming] = [
        proxyRestart: .live,
        // Its own row-level tag in the mockup ("takes effect next launch")
        // rather than the section's "applied live": TcrBar reads this
        // preference once, at `applicationDidFinishLaunching`, not on every
        // poll — so `.boot` is the honest word even though nothing here is
        // server config. `SettingsRowBadge` has no fourth case for "next
        // app launch specifically", and boot-time is the closer of the two
        // to what a reader should expect: the change is inert until
        // something restarts.
        startServerAtLaunch: .boot,
        pollInterval: .live,
        launchAtLogin: .live,
        keepAwake: .live,
        showReadyCount: .live,
        showRunningToolCount: .live,
        openOnTab: .live,
        textSize: .readOnly,
        groupParked: .live,
        // Neither has a write path through this app today — `GroupController`
        // covers add/remove/removeAll/park, not reserve or control-eligibility
        // — so per the bridge these are read-only rows with a hint, not a
        // `.boot` row implying a control this window does not actually offer.
        groupReserved: .readOnly,
        groupMayServeAsControl: .readOnly,
        groupColor: .readOnly,
        groupMembers: .readOnly,
        switchThreshold: .boot,
        controlReserve: .boot,
        fableWeeklyThreshold: .boot,
        resetUrgencyTier: .boot,
        sessionAffinity: .boot,
        controlAccount: .boot,
        controlPooled: .boot,
        accountThrottle: .boot,
        fleetThrottle: .boot,
        pacing: .boot,
        usageRetentionDays: .boot,
        http1Only: .boot,
        checkNow: .live,
        whatsNew: .live,
        checkAutomatically: .readOnly,
        runningServer: .readOnly,
        installedCli: .readOnly,
        updatingReplacesBoth: .readOnly,
    ]

    /// `nil` only for a key nobody registered above — which
    /// ``SettingsRowBadgeTests`` treats as a failure, not a case a pane is
    /// allowed to silently fall back from.
    public static func timing(for key: String) -> SettingsRowTiming? {
        timing[key]
    }
}
