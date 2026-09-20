import AppKit
import Darwin
import TcrBarCore

/// The signals a native crash (an uncaught `NSException` converts to
/// `SIGABRT` via `abort()`, the runaway-layout crash this pairs with among
/// them) or a fatal Swift-runtime trap raises. Installed once, in
/// ``TcrBarEntry/main()``, before anything that could spawn the child these
/// handlers exist to stop from being orphaned.
private let abnormalTerminationSignals: [Int32] = [
    SIGABRT, SIGSEGV, SIGILL, SIGBUS, SIGFPE,
]

/// The handler itself. A free, non-capturing function because `signal(2)`
/// takes a C function pointer, which cannot close over anything — this is
/// why the pid it needs lives in a static var
/// (`ServerController.supervisedChildPID`) rather than being passed in.
///
/// Everything it does beyond the one call into
/// `AbnormalTerminationGuard.terminateSupervisedChild(pid:)` is restoring
/// the default disposition and re-raising, which is what makes the process
/// still crash, still print the same report, and still exit with the same
/// code it would have without this handler installed — the only thing this
/// adds is that the child is no longer orphaned first.
private func tcrbarHandleAbnormalTermination(_ signalNumber: Int32) {
    AbnormalTerminationGuard.terminateSupervisedChild(
        pid: pid_t(ServerController.supervisedChildPID))
    signal(signalNumber, SIG_DFL)
    raise(signalNumber)
}

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
        // Before anything else — including the four harness paths below,
        // none of which spawn a child, but a handler installed after the
        // spawn point would race the very crash it exists to catch.
        for signalNumber in abnormalTerminationSignals {
            signal(signalNumber, tcrbarHandleAbnormalTermination)
        }
        // Installed alongside them, and it runs FIRST: an uncaught
        // `NSException` unwinds through this handler before it reaches
        // `abort()`, which is what raises the `SIGABRT` the loop above
        // catches. So this logs, and then the signal handler still gets to
        // stop the child being orphaned — the two do not compete.
        //
        // It exists because the crash report does not carry the reason
        // string. `UncaughtExceptionReport` documents what that cost when the
        // runaway-layout crash had to be diagnosed from frames alone. The
        // closure captures nothing, which is required: the parameter is a C
        // function pointer.
        NSSetUncaughtExceptionHandler { exception in
            for line in UncaughtExceptionReport.lines(
                name: exception.name.rawValue,
                reason: exception.reason,
                callStack: exception.callStackSymbols)
            {
                NSLog("%@", line)
            }
        }
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
        if let directory = RenderSettings.requestedDirectory() {
            _ = NSApplication.shared
            RenderSettings.run(into: directory)  // exits
        }
        if let directory = RenderMark.requestedDirectory() {
            _ = NSApplication.shared
            RenderMark.run(into: directory)  // exits
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
        // The other flag that builds a status item, and for the same reason:
        // what one panel open costs cannot be measured without opening one.
        // It reads `tcr` with read verbs only and never starts a server.
        if MeasureOpen.requested() {
            MeasureOpen.run()  // exits
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
        // The one call that can open the release-notes window unbidden: only
        // after an update (a version the operator has not seen notes for), and
        // only once the notes actually loaded. Never awaited — a hung network
        // must not hold up launch — and never reached by the render or probe
        // harnesses, which build the shell without this delegate.
        Task {
            if await shell.whatsNew.checkAfterLaunch() {
                shell.whatsNewWindow.present()
            }
        }
        // Was `FleetView.onAppear`, which under `MenuBarExtra` meant the fleet
        // was not polled until the panel had been opened once — the menu-bar
        // glyph sat at its `.pending` gauge until then.
        shell.poller.start()
        // And the knocks, on the same terms and for a stronger version of the
        // same reason: the Peers tab's own reader lives and dies with the tab,
        // so with the panel closed, which is almost always, nothing in this
        // app knows a Mac is asking to connect. This is what puts the mark on
        // the bar and what the notifier reads.
        shell.knocks.start()
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
            // The first launch after an UPDATE takes the port; every other
            // launch stands down to an incumbent. When TcrBar quits for an
            // update it stops its own child, so normally nothing holds the port
            // and the two are the same spawn. The case this exists for is a
            // proxy that outlived the old app — one it never supervised, or
            // one it failed to stop — which the old `--no-replace` start
            // quietly yielded to, leaving the just-updated app showing
            // "Take over port…" for the operator to press by hand (0.2.35).
            // The operator chose the update; the proxy it replaces is by
            // definition the pre-update one.
            if LaunchVersionMarker().noteLaunch(current: AppBuild.shortVersion) {
                NSLog("TcrBar: first launch of \(AppBuild.shortVersion ?? "?") — taking the port")
                shell.server.startTakingOverPort()
            } else {
                shell.server.start()
            }
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

    /// The SECOND scheme this app answers: `tcr://peer/join?v=1&nk=…[&jk=…]`,
    /// decision row 11's share link.
    ///
    /// Two schemes and not one, because they are two different things.
    /// `tcrbar://` is the CLI asking this app to act on the operator's behalf;
    /// `tcr://` is a person pasting a CREDENTIAL that happens to open this
    /// app. Sharing a namespace would put an update check and an office
    /// network key in one switch.
    ///
    /// **The whole URL goes to `tcr peer join --stdin`, and never into argv.**
    /// `nk` is the network key and `jk` a one-use join key; argv is readable
    /// by every process on this Mac through `ps` and lands in this process's
    /// crash reports. The CLI owns what a link MEANS, which key sets what,
    /// that a spent `jk` still sets `nk`, so this hands the string over
    /// rather than unpacking it.
    ///
    /// What reaches the log is ``PeerJoinLink/redacted(_:)``: the path, and
    /// which kinds of key rode along. Never the URL, never a key. Silence is
    /// not an option here, a link that did nothing quietly is
    /// indistinguishable from a broken handler, and neither is the link
    /// itself, because the system log is readable by other processes too.
    ///
    /// **A shape refusal (wrong scheme, wrong path, no key at all) never
    /// raises the confirmation sheet.** ``PeerJoinLink/invocation(for:)``
    /// already answers that for free before this does anything, and a sheet
    /// asking "join this mesh?" over a link that was never going anywhere
    /// would train an operator to stop reading it. Only a link that WOULD
    /// actually set something asks first, via ``PeerJoinConfirmation``.
    private func handleJoinLink(_ url: URL) {
        let shape = PeerJoinLink.redacted(url)
        NSLog("TcrBar: %@ received", shape)
        guard case .success = PeerJoinLink.invocation(for: url) else {
            let outcome = PeerController.join(link: url)
            NSLog("TcrBar: %@: %@", shape, outcome)
            return
        }
        Task.detached(priority: .userInitiated) {
            let hasExistingKey = Self.networkKeyIsCurrentlySet()
            let confirmed = await PeerJoinConfirmation.confirm(url: url, hasExistingKey: hasExistingKey)
            guard confirmed else {
                NSLog("TcrBar: %@: cancelled at the confirmation sheet", shape)
                return
            }
            // The sheet already told the operator a key would be replaced
            // (`hasExistingKey`); Join means it, so the command carries the
            // flag `tcr peer join` requires to act on that.
            let outcome = PeerController.join(link: url, replace: hasExistingKey)
            NSLog("TcrBar: %@: %@", shape, outcome)
        }
    }

    /// The other path under the `tcr://` scheme:
    /// `tcr://peer/moved?v=1&r=…`, a Mac this one already trusts saying where
    /// it can be reached now.
    ///
    /// **It is not a join and must never be able to become one.** This link
    /// carries no credential and joins nothing; the most it leads to is a few
    /// addresses added to a row this Mac already pinned, and `tcr` is what
    /// holds that bound. The two paths are told apart by
    /// ``PeerMovedLink/route(_:)`` before either handler sees the URL, because
    /// the join handler used to sit behind the whole scheme and a new path
    /// with no decision in front of it would have piped a moved link into the
    /// verb that sets this Mac's network key.
    ///
    /// **A shape refusal never raises an alert.** A link that carries nothing
    /// sealed is answered in the log and nowhere else, the same split the join
    /// handler makes: an alert over a link that was never going anywhere
    /// trains an operator to stop reading alerts.
    ///
    /// What reaches the log is ``PeerMovedLink/redacted(_:)``: the path, and
    /// whether anything sealed rode along. Never the record. Anyone holding
    /// the string can replay it while it is still good, and the system log is
    /// readable by other processes on this Mac.
    private func handleMovedLink(_ url: URL) {
        let shape = PeerMovedLink.redacted(url)
        NSLog("TcrBar: %@ received", shape)
        if case .failure(let refusal) = PeerMovedLink.invocation(for: url) {
            NSLog("TcrBar: %@: %@", shape, PeerMovedLink.sentence(for: refusal))
            return
        }
        Task.detached(priority: .userInitiated) {
            let outcome = await PeerMovedConfirmation.readAskKeep(url: url)
            NSLog("TcrBar: %@: %@", shape, outcome)
        }
    }

    /// `tcr peer network-key show`, blocking, off the main thread: the one
    /// fact ``PeerJoinConfirmation`` needs and this app never reads any other
    /// way (the peers file's own `network_key` field never crosses `peer ls`
    /// or `status --json`). `false` on any failure to resolve or run `tcr`:
    /// the sheet then reads as "nothing to replace" rather than blocking a
    /// legitimate join on a `tcr` this build cannot find; that failure
    /// surfaces properly a moment later, when the join itself is attempted.
    private nonisolated static func networkKeyIsCurrentlySet() -> Bool {
        guard case .success(let executable) = TcrTool.resolve() else { return false }
        guard
            let output = try? TcrTool.run(
                executable: executable, arguments: PeerCommand.networkKeyShow)
        else { return false }
        return PeerJoinLink.networkKeyIsSet(
            output: String(data: output.stdout, encoding: .utf8) ?? "")
    }

    /// Host-based, not path-based: `URL(string:)` parses `tcrbar://check-for-updates`
    /// with `host == "check-for-updates"` and an empty path. An unrecognised URL is
    /// logged rather than silently dropped, because a mistyped scheme call that
    /// does nothing is indistinguishable from a broken updater.
    private func handle(_ url: URL) {
        if url.scheme?.lowercased() == PeerJoinLink.scheme {
            switch PeerMovedLink.route(url) {
            case .moved:
                handleMovedLink(url)
            case .join, .neither:
                handleJoinLink(url)
            }
            return
        }
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
