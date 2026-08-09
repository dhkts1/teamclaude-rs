import AppKit
import SwiftUI
import TcrBarCore

/// The real entry point.
///
/// `TcrBarApp` cannot carry `@main` itself, because the render harness has to run
/// and exit BEFORE SwiftUI installs a menu-bar item or the poller starts. Doing it
/// from `applicationDidFinishLaunching` would flash an icon in the menu bar and
/// fire a `tcr` subprocess on a machine that only asked for PNGs.
@main
enum TcrBarEntry {
    @MainActor
    static func main() {
        if let directory = RenderStates.requestedDirectory() {
            // AppKit needs to exist before anything can be rasterised, but the
            // app is never activated and no window or status item is created.
            _ = NSApplication.shared
            RenderStates.run(into: directory)  // exits
        }
        if let directory = AppIcon.requestedDirectory() {
            _ = NSApplication.shared
            AppIcon.writeIconSet(to: directory)  // exits
        }
        TcrBarApp.main()
    }
}

/// TcrBar — a menu-bar accessory for the `tcr` rotating proxy.
///
/// `LSUIElement` is set in the bundle's Info.plist, so there is no Dock icon and
/// no main window: the whole app is the `MenuBarExtra`.
struct TcrBarApp: App {
    @NSApplicationDelegateAdaptor(AppDelegate.self) private var delegate
    @StateObject private var poller = StatusPoller()
    @StateObject private var server = ServerController()
    @StateObject private var loginItem = LoginItem()
    @StateObject private var accounts = AccountController()
    @StateObject private var updater = Updater()

    /// Bring the proxy up when the app starts.
    ///
    /// Opt-in, and deliberately not default-on. Paired with "Launch at login" it
    /// means the proxy is simply always up — but it also makes Quit expensive,
    /// because once TcrBar supervises the server, quitting stops it. Turning that
    /// on should be a decision the operator made, not a surprise on first launch.
    ///
    /// Safe by construction: this is `start()`, which passes `--no-replace`, so a
    /// proxy that is already serving is never disturbed. If one is, the spawn
    /// reports "already running" and nothing happens.
    @AppStorage("startServerAtLaunch") private var startServerAtLaunch = false

    /// Fires once per process, not once per panel open. `onAppear` on the
    /// `MenuBarExtra` content runs every time the menu is opened, so without this
    /// the app would attempt a spawn on every click.
    @State private var didAttemptLaunchStart = false

    var body: some Scene {
        MenuBarExtra {
            FleetView(
                poller: poller,
                server: server,
                loginItem: loginItem,
                accounts: accounts,
                updater: updater,
                startServerAtLaunch: $startServerAtLaunch
            )
            .onAppear {
                poller.start()
                delegate.server = server
                // macOS owns the login-item bit; re-read it every time the
                // panel opens so a revocation in System Settings shows up.
                loginItem.refresh()
                if startServerAtLaunch, !didAttemptLaunchStart {
                    didAttemptLaunchStart = true
                    server.start()
                }
            }
        } label: {
            MenuBarLabel(state: poller.state)
                // Deliberately on the LABEL, not on the panel content.
                //
                // The delegate is what receives `tcrbar://check-for-updates`,
                // and the updater is owned by the app, so the delegate has to be
                // handed a reference before any URL can arrive. The panel's
                // `onAppear` fires only when someone opens the menu — a URL sent
                // to an app whose panel has never been opened would then find a
                // nil updater and do nothing. The label is drawn at launch,
                // which is the moment the app becomes reachable at all.
                .onAppear { delegate.updater = updater }
        }
        .menuBarExtraStyle(.window)
    }
}

/// Two jobs: a child process TcrBar spawned must not outlive it, and the app's
/// `tcrbar://` URLs land here.
///
/// `terminateSupervisedChildOnQuit()` is a no-op unless *this app* spawned the
/// server — an incumbent proxy is never signalled.
@MainActor
final class AppDelegate: NSObject, NSApplicationDelegate {
    var server: ServerController?
    /// Handed over by `TcrBarApp` when the menu-bar label first appears, which is
    /// at launch. Optional because the delegate is constructed by AppKit before
    /// any SwiftUI scene exists, not because it is expected to stay nil.
    var updater: Updater?

    func applicationWillTerminate(_ notification: Notification) {
        server?.terminateSupervisedChildOnQuit()
    }

    /// The URL contract with the `tcr` CLI: `tcrbar://check-for-updates` runs the
    /// same user-initiated check the panel's button runs.
    ///
    /// The scheme is declared in `CFBundleURLTypes` by `scripts/build-tcrbar.sh`;
    /// a bundle built any other way is not registered with LaunchServices and no
    /// URL will ever reach this method. `LSUIElement` does not change that —
    /// an accessory app is a perfectly ordinary URL handler.
    ///
    /// Host-based, not path-based: `URL(string:)` parses `tcrbar://check-for-updates`
    /// with `host == "check-for-updates"` and an empty path. An unrecognised URL
    /// is logged rather than silently dropped, because a typo'd scheme call that
    /// does nothing is indistinguishable from a broken updater.
    func application(_ application: NSApplication, open urls: [URL]) {
        for url in urls {
            guard url.scheme == "tcrbar" else {
                NSLog("TcrBar: ignoring URL with unexpected scheme: %@", url.absoluteString)
                continue
            }
            switch url.host {
            case "check-for-updates":
                NSLog("TcrBar: tcrbar://check-for-updates received")
                guard let updater else {
                    NSLog("TcrBar: no updater is available yet — check ignored")
                    continue
                }
                updater.checkForUpdates()
            default:
                NSLog("TcrBar: unhandled tcrbar URL: %@", url.absoluteString)
            }
        }
    }
}

/// The glyph in the menu bar: fleet *capacity*, not the worst account.
///
/// It answers one question — can I work right now — and it is deliberately not
/// worst-account-wins. In a rotating pool spent accounts are the mechanism
/// working, so a worst-wins glyph pinned itself to the alarm state whenever any
/// one of thirteen accounts was spent, which is nearly always. The mapping lives
/// in `Fleet.capacityGlyphState`; this view does no logic. A failed read shows a
/// warning glyph rather than a healthy-looking gauge.
struct MenuBarLabel: View {
    let state: PollState

    var body: some View {
        switch state {
        case .loaded(let fleet):
            Image(systemName: Tok.glyph(for: fleet.capacityGlyphState))
        case .pending:
            Image(systemName: "gauge.with.dots.needle.33percent")
        case .toolMissing, .commandFailed, .undecodable:
            Image(systemName: Tok.unreadableGlyph)
        }
    }
}
