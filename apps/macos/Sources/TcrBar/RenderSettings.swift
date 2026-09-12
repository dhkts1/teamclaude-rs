import AppKit
import SwiftUI
import TcrBarCore

/// Rasterise the Settings window, one PNG per pane, in-process, then exit.
///
/// Same reasoning as ``RenderStates``: `ImageRenderer` needs no Screen
/// Recording permission and draws the real view with the real tokens, and
/// this window's genuine failure mode (S1, `docs/design/panel-tabs-review.md`)
/// was exactly a fact only a render could show — a sidebar selection that drew
/// every section from every pane underneath it regardless of which was
/// "selected". `swift test` cannot see that; a screenshot of each pane can.
///
/// **Captures the DETAIL pane content directly, not `SettingsRootView`.**
/// Measured, not assumed: the first version of this harness rendered the
/// whole `NavigationSplitView` (sidebar, toolbar, detail) and every pane past
/// the first came back as AppKit's generic "cannot display this content"
/// glyph — a yellow circle-slash, not a rendering failure with an error on
/// stderr, so `written == attempted` reported a false 8/8. `NavigationSplitView`
/// plus a `.toolbar` needs a real `NSWindow`/`NSToolbar` host, which
/// `ImageRenderer` does not provide (`RenderStates`'s own views never use
/// either, which is why that harness never hit this). The sidebar and
/// toolbar chrome is therefore **not verified by this harness** — same
/// admitted gap `RenderStates` already carries for AppKit controls
/// (`--render-states`'s own header: "a `.checkbox` toggle … `ImageRenderer`
/// does not draw those at all") — and needs the real running window.
///
/// ## Usage
///
///     TcrBar.app/Contents/MacOS/TcrBar --render-settings /tmp/tcrbar-settings
///
/// Writes one PNG per pane per appearance and exits without ever showing a
/// menu-bar item, polling `tcr`, or touching a server.
enum RenderSettings {
    static let flag = "--render-settings"

    static func requestedDirectory(_ arguments: [String] = CommandLine.arguments) -> URL? {
        guard let i = arguments.firstIndex(of: flag), i + 1 < arguments.count else { return nil }
        return URL(fileURLWithPath: arguments[i + 1])
    }

    enum Appearance: String, CaseIterable {
        case dark, light
        var nsAppearance: NSAppearance? {
            NSAppearance(named: self == .dark ? .darkAqua : .aqua)
        }
    }

    @MainActor
    static func run(into directory: URL) -> Never {
        do {
            try FileManager.default.createDirectory(
                at: directory, withIntermediateDirectories: true)
        } catch {
            FileHandle.standardError.write(
                Data("cannot create \(directory.path): \(error)\n".utf8))
            exit(1)
        }

        var written = 0
        var attempted = 0
        for tab in SettingsTab.allCases {
            for appearance in Appearance.allCases {
                attempted += 1
                if render(tab, appearance: appearance, into: directory) { written += 1 }
            }
        }

        print("\nrendered \(written)/\(attempted) images into \(directory.path)")
        exit(written == attempted ? 0 : 1)
    }

    /// The same switch `SettingsDetailView` (`SettingsView.swift`) makes,
    /// reproduced here rather than reused: that type is `private` to its own
    /// file and wraps its result in `.navigationTitle`, which is meaningless
    /// (and untestable) outside a real `NavigationSplitView` host.
    @ViewBuilder
    private static func paneView(
        for tab: SettingsTab, dependencies: SettingsDependencies
    ) -> some View {
        switch tab {
        case .general: GeneralSettingsPane(dependencies: dependencies)
        case .menuBar: MenuBarSettingsPane(dependencies: dependencies)
        case .groupsRotation: GroupsRotationSettingsPane(dependencies: dependencies)
        case .updates: UpdatesSettingsPane(dependencies: dependencies)
        }
    }

    /// Two groups — one parked, one not — so the Groups & Rotation pane shows
    /// both a real member count and a real parked toggle state, the same way
    /// `RenderStates`'s own fixtures avoid an all-healthy fleet that cannot
    /// distinguish "wired correctly" from "never wired at all".
    private static func fixtureAccounts() -> [Account] {
        [
            fixtureAccount("alice@example.com", groups: ["dev"], groupColors: ["dev": "#0a84ff"]),
            fixtureAccount("bob@example.com", groups: ["dev"], groupColors: ["dev": "#0a84ff"]),
            fixtureAccount(
                "carol@example.com", groups: ["henry-team"], parkedGroups: ["henry-team"],
                groupColors: ["henry-team": "#32d74b"]),
        ]
    }

    @MainActor
    private static func render(
        _ tab: SettingsTab, appearance: Appearance, into directory: URL
    ) -> Bool {
        let previous = NSAppearance.current
        NSAppearance.current = appearance.nsAppearance
        // `NSAppearance.current` alone resolves `Tok`'s own dynamic
        // `NSColor` closures (measured: tag text and borders came out
        // correctly tinted for dark even before this line existed) but NOT
        // `Form(.grouped)`'s native system materials — those follow
        // `NSApplication.appearance`, which stayed at the system's real
        // appearance regardless, so the first attempt at this fix rendered
        // dark-appropriate text on a light-appearance background. Both must
        // be set for one coherent capture.
        // **Measured, and only partially effective**: with both lines set,
        // `Tok`'s own dynamic colours (every tag, every hint) correctly
        // switch per appearance, but `Form(.grouped)`'s native system
        // material (the light/dark grouped-row background) does not — this
        // off-screen, never-activated, never-key window does not walk the
        // same effective-appearance path a real on-screen window does. The
        // dark PNGs from this harness are therefore accurate for every
        // `Tok`-drawn element and NOT proof of the native chrome's dark
        // appearance; that needs the real running window
        // (`SettingsWindowController`, opened by hand).
        let previousAppAppearance = NSApp.appearance
        NSApp.appearance = appearance.nsAppearance
        defer {
            NSAppearance.current = previous
            NSApp.appearance = previousAppAppearance
        }

        let fleet = Fleet(accounts: fixtureAccounts())
        let dependencies = SettingsDependencies(
            poller: StatusPoller(pinnedState: .loaded(fleet), lastPollAt: Date()),
            server: ServerController(),
            loginItem: LoginItem(),
            awake: AwakeController.harness(),
            preference: LaunchPreference(),
            countsPreference: MenuBarCountsPreference(),
            runningToolsPreference: ShowRunningToolsPreference(),
            defaultTabPreference: DefaultTabPreference(),
            groupController: GroupController(),
            updater: Updater(startingUpdater: false),
            onWhatsNew: {})

        // The detail pane's own content, at the window's real inner width —
        // 660 total minus the fixed 200pt sidebar (`SettingsView.swift`'s own
        // `.frame(width: 200)`).
        //
        // **`ImageRenderer` alone renders this blank.** Measured across three
        // attempts (`.fixedSize()`, a bare `.frame`, `.frame` plus a matching
        // `proposedSize`): all three produced a pixel-uniform white PNG, byte-
        // identical across every pane and both appearances. `Form(.grouped)`
        // is `NSTableView`-backed on macOS, and `ImageRenderer` runs its
        // layout headless, off any real window — `FleetView`'s own plain
        // `ScrollView`+`VStack` rasterises fine that way (`RenderStates`
        // already proves it), but a table view's rows are drawn through
        // AppKit's real display path, which a window-less render never
        // triggers. So this hosts the view in a real (off-screen, non-key,
        // never-shown-to-the-operator) `NSWindow` and captures it with
        // `NSView.cacheDisplay(in:to:)` — the same mechanism screen-capture
        // tools use, and the one path that actually walks the table view's
        // `draw(_:)` — instead of `ImageRenderer`.
        let hostedView = paneView(for: tab, dependencies: dependencies)
            .environment(\.colorScheme, appearance == .dark ? .dark : .light)
            .frame(width: 460, height: 540)

        let hosting = NSHostingView(rootView: hostedView)
        hosting.frame = NSRect(x: 0, y: 0, width: 460, height: 540)

        let window = NSWindow(
            contentRect: hosting.frame, styleMask: [.borderless], backing: .buffered,
            defer: false)
        // Off-screen — this process never shows the operator a window, the
        // same guarantee `RenderStates` gives for its own harness.
        window.setFrameOrigin(NSPoint(x: -10000, y: -10000))
        window.appearance = appearance.nsAppearance
        window.contentView = hosting
        window.orderFrontRegardless()
        hosting.layoutSubtreeIfNeeded()
        // One run-loop turn so AppKit actually lays out and backs the table
        // view before the capture — `layoutSubtreeIfNeeded()` alone left the
        // same blank result in an earlier attempt at this same fix.
        RunLoop.current.run(until: Date().addingTimeInterval(0.05))

        let name = "\(tab.rawValue)-\(appearance.rawValue).png"
        guard let rep = hosting.bitmapImageRepForCachingDisplay(in: hosting.bounds)
        else {
            window.close()
            FileHandle.standardError.write(Data("render failed: \(name)\n".utf8))
            return false
        }
        hosting.cacheDisplay(in: hosting.bounds, to: rep)
        window.close()
        guard let png = rep.representation(using: .png, properties: [:]) else {
            FileHandle.standardError.write(Data("render failed: \(name)\n".utf8))
            return false
        }

        let url = directory.appendingPathComponent(name)
        do {
            try png.write(to: url)
            print("  \(name)  \(Int(hosting.bounds.width))x\(Int(hosting.bounds.height))pt")
            return true
        } catch {
            FileHandle.standardError.write(Data("write failed \(name): \(error)\n".utf8))
            return false
        }
    }
}

/// Hand-built account with the fields this harness's fixtures touch — same
/// shape as `GroupSummaryTests`'s own `summaryAccount` fixture.
private func fixtureAccount(
    _ name: String,
    groups: [String]?,
    reservedGroups: [String]? = nil,
    parkedGroups: [String]? = nil,
    groupColors: [String: String]? = nil
) -> Account {
    Account(
        name: name,
        priority: 1,
        status: "active",
        disabled: false,
        quota: 0.2,
        quotaState: .ok,
        fiveHour: 0.2,
        sevenDay: 0.2,
        sevenDayOi: nil,
        held: [],
        requests: 10,
        inputTokens: 1000,
        outputTokens: 500,
        cacheReadTokens: 200,
        cacheHitRatio: 0.9,
        probeStatus: .ok,
        probeError: nil,
        lastStreamError: nil,
        streamErrorCount: 0,
        source: .live,
        serverSha: "abc1234",
        serverDirty: false,
        groups: groups,
        reservedGroups: reservedGroups,
        parkedGroups: parkedGroups,
        groupColors: groupColors
    )
}
