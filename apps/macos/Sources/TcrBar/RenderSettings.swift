import AppKit
import SwiftUI
import TcrBarCore

/// Rasterise the Settings window, one PNG per pane, in-process, then exit.
///
/// ## Two rendering approaches tried and rejected before this one
///
/// **`ImageRenderer` on the whole `NavigationSplitView`** (attempt 1):
/// every pane past the first came back as AppKit's generic "cannot display
/// this content" glyph — a yellow circle-slash, not a rendering failure with
/// an error on stderr, so `written == attempted` reported a false 8/8.
/// `NavigationSplitView` plus a `.toolbar` needs a real `NSWindow`/`NSToolbar`
/// host, which `ImageRenderer` never provides.
///
/// **`ImageRenderer` on each pane's bare `Form`, no split view** (attempt 2):
/// fixed the placeholder, but produced a pixel-uniform blank/white PNG —
/// `Form(.grouped)` is `NSTableView`-backed, and `ImageRenderer`'s headless
/// layout pass never walks a table view's real `draw(_:)` path either
/// (`FleetView`'s own plain `ScrollView`+`VStack` DOES rasterise fine via
/// `ImageRenderer` — `RenderStates` already proves it — so this is specific
/// to table/list-backed SwiftUI, not scroll content generally). A follow-up
/// fix hosted the bare pane in an OFF-SCREEN `NSWindow` and captured with
/// `NSView.cacheDisplay(in:to:)`: the pane's own text and `Tok`-drawn badges
/// rendered, but the window never got real AppKit compositing (off-screen,
/// never key, never main) — dark captures showed dark-tinted TEXT on a
/// LIGHT background, which is not merely "wrong chrome colour", it is text
/// that is barely legible against its own background. Read, not assumed: a
/// second reviewer caught this by reading `groupsRotation-dark.png` and
/// finding badges and toggle knobs floating on blank white with no row
/// labels, no section cards and no sidebar at all.
///
/// **This version: the REAL `SettingsRootView` (sidebar, toolbar, detail —
/// everything `SettingsWindowController` shows) in a real window, positioned
/// ON a connected screen and given one real display cycle before capture.**
/// The window is `.orderFrontRegardless()`ed rather than skipped, because
/// that ordering — plus a real screen origin — is what makes AppKit's
/// vibrancy/table-view backing stores actually get created; an off-screen
/// origin was tried first and left the same blank/mistinted result attempt
/// 2 already describes. `NSApp.setActivationPolicy` is never touched and no
/// status item or server is ever created (this whole harness runs before
/// `AppDelegate` exists — see `TcrBarEntry.main()`), so this never registers
/// as the running menu-bar app or starts a proxy; the window is closed again
/// immediately after each capture.
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

    /// 660×581 — the window's own real minimum, per `SettingsWindowController`
    /// and the design review's own "Applied" note
    /// (`docs/design/panel-tabs-review.md`: "window 660×581 with a 200px
    /// sidebar").
    private static let windowSize = NSSize(width: 660, height: 581)

    @MainActor
    private static func render(
        _ tab: SettingsTab, appearance: Appearance, into directory: URL
    ) -> Bool {
        let previous = NSAppearance.current
        NSAppearance.current = appearance.nsAppearance
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

        // A fresh navigation object per capture, not `.shared` — so setting
        // `selectedTab` here can never race or persist against a later call
        // in this same process.
        let navigation = SettingsNavigation()
        navigation.selectedTab = tab

        // The REAL root view — sidebar, toolbar, detail, exactly what
        // `SettingsWindowController` shows — not a bare pane. See this
        // type's own doc-comment for the two rejected approaches that only
        // captured the detail content.
        let rootView = SettingsRootView(dependencies: dependencies, navigation: navigation)
            .environment(\.colorScheme, appearance == .dark ? .dark : .light)

        let hostingController = NSHostingController(rootView: rootView)
        let window = NSWindow(
            contentRect: NSRect(origin: .zero, size: windowSize),
            styleMask: [.titled, .closable, .resizable, .miniaturizable, .fullSizeContentView],
            backing: .buffered,
            defer: false)
        window.appearance = appearance.nsAppearance
        window.contentViewController = hostingController
        window.setFrame(NSRect(origin: .zero, size: windowSize), display: true)
        // A normal on-screen position — centred, the way a real window would
        // open — not the screen's literal (0,0) origin. `NSWindow.center()`
        // measured, not assumed: an earlier attempt set the origin to the
        // screen's own `minX`/`minY` (AppKit's bottom-LEFT origin), which put
        // the window's bottom edge at the very bottom of the display, mostly
        // hidden behind the Dock — the captured PNG showed only a drop-shadow
        // gradient in two corners, nothing else. `orderFrontRegardless()`
        // rather than `makeKeyAndOrderFront` and `NSApp.activate` never
        // called — this must never take key focus or activate the app
        // (`NSApp.setActivationPolicy` is never touched anywhere in this
        // file, so it stays `.accessory` throughout).
        window.center()
        window.orderFrontRegardless()
        window.layoutIfNeeded()
        window.contentView?.layoutSubtreeIfNeeded()
        // Several run-loop turns so the window server actually composites a
        // frame before capture.
        for _ in 0..<5 {
            RunLoop.current.run(until: Date().addingTimeInterval(0.05))
        }

        let name = "\(tab.rawValue)-\(appearance.rawValue).png"
        // `NSView.cacheDisplay`/`ImageRenderer` both draw OFFSCREEN, bypassing
        // the window server entirely — measured across every attempt in this
        // file's own doc-comment, neither one reliably reproduces what
        // `Form(.grouped)`'s native table/vibrancy backing actually composites.
        // `CGWindowListCreateImage` instead asks the WINDOW SERVER for the
        // pixels it already composited for this exact window — the same
        // mechanism `screencapture -l <windowNumber>` uses. Capturing a
        // window this PROCESS OWNS needs no Screen Recording permission
        // (that gate is for capturing another process's windows); only this
        // process's own window is ever named here.
        guard
            let cgImage = CGWindowListCreateImage(
                .null, .optionIncludingWindow, CGWindowID(window.windowNumber),
                [.boundsIgnoreFraming, .bestResolution])
        else {
            window.close()
            FileHandle.standardError.write(Data("render failed: \(name)\n".utf8))
            return false
        }
        window.close()
        let rep = NSBitmapImageRep(cgImage: cgImage)
        guard let png = rep.representation(using: .png, properties: [:]) else {
            FileHandle.standardError.write(Data("render failed: \(name)\n".utf8))
            return false
        }

        let url = directory.appendingPathComponent(name)
        do {
            try png.write(to: url)
            print("  \(name)  \(cgImage.width)x\(cgImage.height)px")
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
