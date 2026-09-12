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

    @MainActor
    private static func render(
        _ tab: SettingsTab, appearance: Appearance, into directory: URL
    ) -> Bool {
        let previous = NSAppearance.current
        NSAppearance.current = appearance.nsAppearance
        defer { NSAppearance.current = previous }

        SettingsNavigation.shared.selectedTab = tab

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

        let view =
            SettingsRootView(dependencies: dependencies)
            .environment(\.colorScheme, appearance == .dark ? .dark : .light)
            .frame(width: 660, height: 540)

        let renderer = ImageRenderer(content: view)
        renderer.scale = 2
        renderer.proposedSize = ProposedViewSize(width: 660, height: 540)

        let name = "\(tab.rawValue)-\(appearance.rawValue).png"
        guard let image = renderer.nsImage,
            let tiff = image.tiffRepresentation,
            let rep = NSBitmapImageRep(data: tiff),
            let png = rep.representation(using: .png, properties: [:])
        else {
            FileHandle.standardError.write(Data("render failed: \(name)\n".utf8))
            return false
        }

        let url = directory.appendingPathComponent(name)
        do {
            try png.write(to: url)
            print("  \(name)  \(Int(image.size.width))x\(Int(image.size.height))pt")
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
