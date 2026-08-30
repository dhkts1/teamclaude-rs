import AppKit
import TcrBarCore

/// The real entry point.
///
/// Every harness flag runs and exits BEFORE any UI exists. Doing it from
/// `applicationDidFinishLaunching` would flash an icon in the menu bar and fire a
/// `tcr` subprocess on a machine that only asked for PNGs. That ordering is
/// load-bearing: none of these four paths may create a status item, poll `tcr`,
/// or spawn a server.
@main
enum TcrBarEntry {

    /// `NSApplication.delegate` is a **weak** reference, so a delegate that only
    /// exists as a local in ``main()`` is deallocated before the first callback
    /// and the app comes up with no menu-bar item at all.
    @MainActor private static var delegate: AppDelegate?

    @MainActor
    static func main() {
        // First, and the only one of the four that needs no AppKit at all: it
        // draws nothing, it holds a power assertion and prints.
        if let probe = KeepAwakeProbe.request() {
            KeepAwakeProbe.run(probe)  // exits
        }
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
        // The one flag that does build a status item — it has to, it is the gate
        // on the shell. It builds its own, from a pinned poller, an inert
        // keep-awake and an unstarted updater, and never reaches the delegate
        // below.
        if ShellProbe.requested() {
            ShellProbe.run()  // exits
        }

        let app = NSApplication.shared
        // The bundle already sets `LSUIElement`, so this matches what the app
        // already is: no Dock icon, no main window, no menu bar of its own.
        // Setting it here as well is what makes an unbundled `swift build`
        // binary behave the same way.
        app.setActivationPolicy(.accessory)
        let delegate = AppDelegate()
        Self.delegate = delegate
        app.delegate = delegate
        app.run()
        exit(0)
    }
}

/// Owns the shell, makes sure nothing TcrBar started outlives it — a child
/// process, and a power assertion — and is where the app's `tcrbar://` URLs land.
///
/// `terminateSupervisedChildOnQuit()` is a no-op unless *this app* spawned the
/// server; an incumbent proxy is never signalled. Both controllers are owned
/// outright now rather than handed over from the panel's `onAppear`, so there is
/// no window in which a quit could find them nil.
@MainActor
final class AppDelegate: NSObject, NSApplicationDelegate {
    /// Built in ``applicationDidFinishLaunching(_:)``, and the owner of
    /// everything the app runs on — including the ``Updater`` the `tcrbar://`
    /// handler below reaches for.
    ///
    /// A URL that arrives before the shell exists is REMEMBERED rather than
    /// dropped: launching the app *with* `tcrbar://check-for-updates` is the
    /// ordinary case — that is what happens when the app was not already running
    /// — and the URL event beats the delegate's launch callback. Under the
    /// SwiftUI scene this guard hung off a `var updater: Updater?` handed over
    /// when the menu-bar label first appeared; the shell owns the updater now, so
    /// the same guard hangs off the shell, and it covers the same window.
    private var shell: MenuBarShell? {
        didSet {
            guard checkIsPending, let shell else { return }
            checkIsPending = false
            NSLog("TcrBar: running the update check that arrived before launch finished")
            shell.updater.checkForUpdates()
        }
    }
    private var checkIsPending = false

    func applicationDidFinishLaunching(_ notification: Notification) {
        let shell = MenuBarShell()
        self.shell = shell
        // Was `FleetView.onAppear`, which under `MenuBarExtra` meant the fleet
        // was not polled until the panel had been opened once — the menu-bar
        // glyph sat at its `.pending` gauge until then.
        shell.poller.start()
        // One attempt, once per process.
        //
        // This used to need a `didAttemptLaunchStart` flag because `onAppear`
        // fires on every panel open, so without it the app attempted a spawn on
        // every click. `applicationDidFinishLaunching` fires exactly once, so
        // the guard is now structural rather than a variable — the reasoning is
        // kept here because deleting the flag would otherwise delete the reason
        // it existed with it.
        //
        // Safe by construction either way: this is `start()`, which spawns
        // `tcr server --headless --no-replace` (`ServerController.safeArguments`).
        // Standing down rather than disturbing a proxy that is already serving is
        // `tcr`'s default; `--no-replace` only restates it for an older binary.
        // `--headless` is the flag that matters here — without it the child dies
        // on startup trying to put a TUI on a pipe.
        if shell.preference.startServerAtLaunch {
            shell.server.start()
        }
        // Same shape, same place, same once-per-process guarantee: re-take the
        // power assertions if that is how the operator left them. A reboot is
        // exactly when a machine meant to stay up for long runs would otherwise
        // come back asleep-capable with nothing on screen having changed.
        //
        // After `poller.start()` rather than before it only because the fleet is
        // the thing worth being quickest about; the assertions are not racing
        // anything. `AwakeController.restoreFromPreference` is a no-op unless the
        // stored intent is ON.
        shell.awake.restoreFromPreference()
    }

    func applicationWillTerminate(_ notification: Notification) {
        shell?.server.terminateSupervisedChildOnQuit()
        // The kernel drops every power assertion a process holds when it dies,
        // so this line is not what lets the Mac sleep again. It is here so that
        // "quitting TcrBar releases it" is something this app does rather than
        // something it gets away with.
        shell?.awake.releaseOnQuit()
    }

    /// The URL contract with the `tcr` CLI: `tcrbar://check-for-updates` runs the
    /// same user-initiated check the panel's button runs.
    ///
    /// The scheme is declared in `CFBundleURLTypes` by `scripts/build-tcrbar.sh`;
    /// a bundle built any other way is not registered with LaunchServices and no
    /// URL will ever reach here. `LSUIElement` does not change that — an accessory
    /// app is a perfectly ordinary URL handler.
    func application(_ application: NSApplication, open urls: [URL]) {
        for url in urls {
            handle(url)
        }
    }

    /// Host-based, not path-based: `URL(string:)` parses `tcrbar://check-for-updates`
    /// with `host == "check-for-updates"` and an empty path. An unrecognised URL is
    /// logged rather than silently dropped, because a mistyped scheme call that
    /// does nothing is indistinguishable from a broken updater.
    private func handle(_ url: URL) {
        guard url.scheme == "tcrbar" else {
            NSLog("TcrBar: ignoring URL with unexpected scheme: %@", url.absoluteString)
            return
        }
        switch url.host {
        case "check-for-updates":
            NSLog("TcrBar: tcrbar://check-for-updates received")
            guard let shell else {
                NSLog("TcrBar: no updater yet — the check is queued until launch completes")
                checkIsPending = true
                return
            }
            shell.updater.checkForUpdates()
        default:
            NSLog("TcrBar: unhandled tcrbar URL: %@", url.absoluteString)
        }
    }
}
