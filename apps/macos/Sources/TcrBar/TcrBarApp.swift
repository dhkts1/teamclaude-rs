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
        // on the shell. It builds its own, from a pinned poller and an inert
        // keep-awake, and never reaches the delegate below.
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

/// Owns the shell, and makes sure nothing TcrBar started outlives it — a child
/// process, and a power assertion.
///
/// `terminateSupervisedChildOnQuit()` is a no-op unless *this app* spawned the
/// server; an incumbent proxy is never signalled. Both controllers are owned
/// outright now rather than handed over from the panel's `onAppear`, so there is
/// no window in which a quit could find them nil.
@MainActor
final class AppDelegate: NSObject, NSApplicationDelegate {
    private var shell: MenuBarShell?

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
        // Safe by construction either way: this is `start()`, which passes
        // `--no-replace`, so a proxy that is already serving is never disturbed.
        if shell.preference.startServerAtLaunch {
            shell.server.start()
        }
    }

    func applicationWillTerminate(_ notification: Notification) {
        shell?.server.terminateSupervisedChildOnQuit()
        // The kernel drops every power assertion a process holds when it dies,
        // so this line is not what lets the Mac sleep again. It is here so that
        // "quitting TcrBar releases it" is something this app does rather than
        // something it gets away with.
        shell?.awake.releaseOnQuit()
    }
}
