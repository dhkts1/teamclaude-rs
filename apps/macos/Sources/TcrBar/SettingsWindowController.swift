import AppKit
import SwiftUI
import TcrBarCore

/// The Settings window (gear button in the panel header, `⌘,`).
///
/// Built once, lazily, and reused — the same rule `WhatsNewWindow` follows and
/// for the same reason: a window rebuilt on every open loses its size, its
/// sidebar selection and its key focus, and `isReleasedWhenClosed` (AppKit's
/// default) would deallocate it on the red button and crash or duplicate on
/// the next open.
///
/// `.fullSizeContentView` at construction time, per `macos-settings-ui`'s own
/// reference (`SettingsWindowController.swift` in that skill): the style mask
/// has to be set when the window is made, not injected afterwards, or the
/// liquid-glass corner treatment does not apply.
///
/// TcrBar is `LSUIElement` (`macos-patterns` § "Activation Policy") with no
/// Dock icon of its own, so opening this window brings the app briefly to
/// `.regular` the way `WhatsNewWindow.present()` already forces activation —
/// without it the window opens behind whatever the operator was looking at.
/// There is only ever one auxiliary window family in this app at a time in
/// practice (Settings XOR What's New), so a plain enter/leave pair is enough;
/// a reference count would only matter if both could be open simultaneously,
/// which nothing here does today.
@MainActor
final class SettingsWindowController: NSWindowController, NSWindowDelegate {
    private let dependencies: SettingsDependencies

    init(dependencies: SettingsDependencies) {
        self.dependencies = dependencies
        let window = NSWindow(
            contentRect: NSRect(origin: .zero, size: CGSize(width: 660, height: 540)),
            styleMask: [
                .titled, .closable, .resizable, .miniaturizable,
                .fullSizeContentView,
            ],
            backing: .buffered,
            defer: false
        )
        super.init(window: window)
        configureWindow()
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) {
        fatalError("init(coder:) has not been implemented")
    }

    private func configureWindow() {
        guard let window else { return }
        window.title = "Settings"
        window.titleVisibility = .visible
        window.toolbarStyle = .automatic
        window.isMovableByWindowBackground = true
        window.setFrameAutosaveName("TcrBarSettingsWindow")
        // Per `SettingsView.swift:74-88` in the `macos-settings-ui` reference —
        // the shape S7 (`docs/design/panel-tabs-review.md`) exists to hold to:
        // a fixed 200pt sidebar plus a 540pt-tall detail is the floor, not a
        // starting point to shrink from.
        window.minSize = NSSize(width: 660, height: 540)
        window.center()
        window.delegate = self
        window.contentViewController = NSHostingController(
            rootView: SettingsRootView(dependencies: dependencies))
    }

    /// - Parameter tab: jump straight to a pane — the gear's per-tab menu
    ///   item in the mockup, and `⌘,`'s own default of wherever the window
    ///   was left.
    func show(tab: SettingsTab? = nil) {
        if let tab {
            SettingsNavigation.shared.selectedTab = tab
        }
        showWindow(nil)
    }

    override func showWindow(_ sender: Any?) {
        super.showWindow(sender)
        AppActivationPolicy.enter()
        window?.makeKeyAndOrderFront(nil)
        NSApp.activate(ignoringOtherApps: true)
    }

    func windowWillClose(_ notification: Notification) {
        AppActivationPolicy.leave()
    }
}

/// Everything a Settings pane reads or writes, gathered in one struct so
/// `SettingsWindowController` and `RenderSettings` build the view the same
/// way `MenuBarShell` and `RenderStates` already build `FleetView` the same
/// way — one initializer, no second wiring path to drift from the first.
struct SettingsDependencies {
    var poller: StatusPoller
    var server: ServerController
    var loginItem: LoginItem
    var awake: AwakeController
    var preference: LaunchPreference
    var countsPreference: MenuBarCountsPreference
    var runningToolsPreference: ShowRunningToolsPreference
    var defaultTabPreference: DefaultTabPreference
    var groupController: GroupController
    var updater: Updater
    var onWhatsNew: () -> Void = {}
}

/// Reference-counted `.accessory` ↔ `.regular` toggling, the same pattern
/// `macos-patterns` gives for a menu-bar-only app opening a window — kept
/// here rather than in `WhatsNewWindow` (which predates this and manages its
/// own activation inline) so the two auxiliary windows share one counter
/// rather than two independent booleans that could disagree about whether
/// the Dock icon should still be showing.
@MainActor
enum AppActivationPolicy {
    private static var count = 0

    static func enter() {
        count += 1
        NSApp.setActivationPolicy(.regular)
        NSApp.activate(ignoringOtherApps: true)
    }

    static func leave() {
        count = max(0, count - 1)
        guard count == 0 else { return }
        NSApp.setActivationPolicy(.accessory)
    }
}
