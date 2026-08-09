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

    /// Whether the status item is in the menu bar right now.
    ///
    /// Binding this turns a hide into OBSERVABLE STATE instead of a scene
    /// teardown: with `isInserted:` bound, SwiftUI writes `false` here when
    /// Control Center hides the item, and the scene stays declared. Unbound, the
    /// scene is simply removed — and an `App` with no scenes left terminates
    /// itself, which is the bug (`TerminationPolicy`).
    ///
    /// Deliberately NOT persisted. An `@AppStorage`-backed value would remember
    /// "hidden" across launches, so the next launch would come up with no icon and
    /// no panel — a running app that looks dead, which is worse than the failure
    /// being fixed. Starting at `true` every launch makes relaunch the recovery.
    @StateObject private var presence = MenuBarPresence.shared

    var body: some Scene {
        MenuBarExtra(isInserted: $presence.isInserted) {
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
        // Cmd-Q has to authorize too.
        //
        // The standard termination command sends a bare `terminate:`, which
        // `applicationShouldTerminate` now refuses — so leaving it in place would
        // ship a keyboard shortcut that silently does nothing. Replacing it keeps
        // the shortcut and the menu item exactly where a user expects them, and
        // routes them through the one door that is allowed to open.
        .commands {
            CommandGroup(replacing: .appTermination) {
                Button("Quit TcrBar") {
                    TerminationPolicy.shared.authorize(.userChoseQuit)
                    NSApplication.shared.terminate(nil)
                }
                .keyboardShortcut("q")
            }
        }
    }
}

/// Is the status item in the menu bar? Owned here rather than by the `App` struct
/// because `AppDelegate` has to reach it, and the delegate exists before any scene
/// does — a hand-off through a view's `onAppear` would never happen on the launch
/// that matters, the one where the icon is hidden and no view ever appears.
///
/// A singleton for that reason alone. Unlike `Updater`, holding one `Bool` starts
/// nothing, so the render harness pays nothing for it.
@MainActor
final class MenuBarPresence: ObservableObject {
    static let shared = MenuBarPresence()

    @Published var isInserted = true

    /// Replay a Control Center hide on demand.
    ///
    ///     defaults write com.github.dhkts1.tcrbar.dev \
    ///       TcrHideMenuBarItemForTesting -bool true
    ///
    /// This exists because the real trigger cannot be driven from a script.
    /// Control Center owns the visibility and holds it in memory: writing
    /// `com.apple.controlcenter "NSStatusItem Visible Item-0"` is simply ignored
    /// by the running process — measured 2026-08-09, the launched app mirrored
    /// `1` straight back over a `0` written seconds earlier. Only the Control
    /// Center UI, or restarting it, moves that state. Without this key the fix
    /// could only be argued for, never demonstrated, and a fix with no
    /// reproduction is a claim.
    ///
    /// It replays BOTH steps of the traced sequence, in order, because the first
    /// one alone turned out to be survivable:
    ///
    ///  1. the status item is removed — the `NSStatusItemChangeVisibilityAction`
    ///     and the scene teardown behind it;
    ///  2. an unrequested in-process `terminate:` with no Apple event behind it —
    ///     the very next line in the log, and the thing that actually killed the
    ///     app, because AppKit's default reply to it is YES.
    ///
    /// Step 1 on its own is not the failure: measured, a build carrying step 1 and
    /// NO `applicationShouldTerminate` survived 60 s, because binding
    /// `isInserted:` keeps the scene declared and nothing ever asks to terminate.
    /// A harness that stopped there would have gone green against a build with the
    /// fix removed — a probe that cannot fail, proving nothing.
    ///
    /// A defaults key rather than an environment variable because `open` does not
    /// forward the environment to the app it launches, and this project already
    /// overrides behaviour that way (`TcrExecutablePath`).
    static let hideForTestingDefaultsKey = "TcrHideMenuBarItemForTesting"

    /// Called from `applicationDidFinishLaunching`, and that timing is not
    /// incidental. Scheduling this from `init` instead does not work: the
    /// singleton is built while the `App` struct's properties initialize, which is
    /// before `NSApplication` runs, and a `Task { @MainActor in … }` enqueued at
    /// that point never resumes — measured, the "is set" line below printed and
    /// neither replay step ever did, 30 s later. `DispatchQueue.main.asyncAfter`
    /// from a launch callback is serviced by the run loop that is by then running.
    func beginHideReplayIfRequested() {
        guard UserDefaults.standard.bool(forKey: Self.hideForTestingDefaultsKey) else { return }
        NSLog(
            "TcrBar: %@ is set — replaying a Control Center hide shortly.",
            Self.hideForTestingDefaultsKey
        )
        DispatchQueue.main.asyncAfter(deadline: .now() + 3) { [self] in
            NSLog("TcrBar: replay step 1 — removing the menu-bar item")
            isInserted = false
            DispatchQueue.main.asyncAfter(deadline: .now() + 1) {
                NSLog("TcrBar: replay step 2 — unrequested terminate: (nothing authorized this)")
                NSApplication.shared.terminate(nil)
            }
        }
    }

    /// Put the icon back. Reachable from `applicationShouldHandleReopen`, so
    /// `open -b <bundle id>` — which is exactly what `tcr ui` runs — is the
    /// recovery when the icon is hidden and the panel cannot be clicked.
    func show() {
        guard !isInserted else { return }
        NSLog("TcrBar: re-inserting the menu-bar item after a reopen request")
        isInserted = true
    }
}

/// Three jobs: a child process TcrBar spawned must not outlive it, the app's
/// `tcrbar://` URLs land here, and — the reason the app survives a hidden icon at
/// all — every termination request is adjudicated here.
///
/// `terminateSupervisedChildOnQuit()` is a no-op unless *this app* spawned the
/// server — an incumbent proxy is never signalled.
@MainActor
final class AppDelegate: NSObject, NSApplicationDelegate {
    var server: ServerController?

    /// Handed over by `TcrBarApp` when the menu-bar label first appears, which is
    /// at launch. Optional because the delegate is constructed by AppKit before
    /// any SwiftUI scene exists, not because it is expected to stay nil.
    ///
    /// A URL that arrives before that hand-off is REMEMBERED rather than dropped:
    /// launching the app *with* `tcrbar://check-for-updates` is the ordinary case
    /// — that is what happens when the app was not already running — and the URL
    /// event beats the first scene by a few milliseconds.
    var updater: Updater? {
        didSet {
            guard checkIsPending, let updater else { return }
            checkIsPending = false
            NSLog("TcrBar: running the update check that arrived before launch finished")
            updater.checkForUpdates()
        }
    }
    private var checkIsPending = false

    /// A logout, restart or shutdown must not be blocked by an accessory app.
    ///
    /// `applicationShouldTerminate` refuses anything unaccounted for, and macOS
    /// asks every app before powering off — so without this the first shutdown
    /// after the fix would stall on a menu-bar utility and blame the user for it.
    /// `willPowerOffNotification` is delivered before those terminate requests go
    /// out, which is what makes recording it here sufficient.
    func applicationDidFinishLaunching(_ notification: Notification) {
        NSWorkspace.shared.notificationCenter.addObserver(
            forName: NSWorkspace.willPowerOffNotification,
            object: nil,
            queue: .main
        ) { _ in
            TerminationPolicy.shared.authorize(.systemIsPoweringOff)
        }
        // Take over the quit Apple event from AppKit.
        //
        // AppKit's own handler calls `terminate:` without telling us anything, and
        // by the time `applicationShouldTerminate` runs the event is no longer
        // "current" — measured, `currentAppleEvent` was nil for an
        // `osascript -e 'quit app id "…"'`, so the refusal stood and the quit
        // timed out after 25 s with the app still alive. Handling the event here
        // authorizes FIRST and then terminates, which cannot race that timing.
        // This is the path a logout's quit takes too.
        NSAppleEventManager.shared().setEventHandler(
            self,
            andSelector: #selector(handleQuitAppleEvent(_:withReplyEvent:)),
            forEventClass: AEEventClass(kCoreEventClass),
            andEventID: AEEventID(kAEQuitApplication)
        )

        MenuBarPresence.shared.beginHideReplayIfRequested()
    }

    @objc private func handleQuitAppleEvent(
        _ event: NSAppleEventDescriptor,
        withReplyEvent reply: NSAppleEventDescriptor
    ) {
        NSLog("TcrBar: quit Apple event received — terminating")
        TerminationPolicy.shared.authorize(.quitEventReceived)
        NSApplication.shared.terminate(nil)
    }

    /// Half the fix, and measured to be sufficient on its own. AppKit's default
    /// answer is YES, and that default is what turned a hidden menu-bar icon into
    /// an app that could not be launched — see `TerminationPolicy` for the trace.
    /// Termination is now refused unless something identifiable asked for it.
    ///
    /// The other half is binding `MenuBarExtra(isInserted:)`, and the two were
    /// separated and measured rather than assumed. Against a reproduction that
    /// kills the unfixed app instantly (exit 0, empty output), a build carrying
    /// ONLY this method survived 60 s, and so did a build carrying only the
    /// binding. Each prevents the death by itself; neither is dead weight, and
    /// the redundancy is deliberate because they fail for different reasons — the
    /// binding depends on SwiftUI keeping a declared-but-absent scene alive, which
    /// is undocumented behaviour, while this depends only on AppKit asking before
    /// it quits.
    func applicationShouldTerminate(_ sender: NSApplication) -> NSApplication.TerminateReply {
        let external = Self.currentEventIsAQuitRequest()
        guard TerminationPolicy.shared.allowsTermination(externalQuitRequest: external) else {
            NSLog(
                "TcrBar: refusing a termination nobody requested — this is what a hidden "
                    + "menu-bar icon looks like. The app stays running; re-show the icon in "
                    + "Control Center, or run `tcr ui` to put it back."
            )
            return .terminateCancel
        }
        let reason = TerminationPolicy.shared.authorization?.rawValue ?? "externalQuitRequest"
        NSLog("TcrBar: terminating (%@)", reason)
        return .terminateNow
    }

    /// Did the terminate currently being handled arrive as a quit Apple event?
    ///
    /// This is the discriminator, and it is what makes the fix safe. Every quit
    /// request from OUTSIDE the process carries one — `osascript -e 'quit app …'`,
    /// the Dock's Quit, Cmd-Q through the standard menu — and none of them run any
    /// of our code first, so none of them could have called `authorize(_:)`. The
    /// status-item teardown carries no Apple event: it is a bare in-process
    /// `terminate:`, so it is the one case that falls through to a refusal.
    private static func currentEventIsAQuitRequest() -> Bool {
        guard let event = NSAppleEventManager.shared().currentAppleEvent else { return false }
        return event.eventClass == AEEventClass(kCoreEventClass)
            && event.eventID == AEEventID(kAEQuitApplication)
    }

    /// `open -b com.github.dhkts1.tcrbar` on an already-running copy — which is
    /// what `tcr ui` does — arrives here. Putting the icon back makes that the
    /// recovery from a hide, without having to kill a perfectly healthy app.
    func applicationShouldHandleReopen(
        _ sender: NSApplication,
        hasVisibleWindows: Bool
    ) -> Bool {
        MenuBarPresence.shared.show()
        return true
    }

    func applicationWillTerminate(_ notification: Notification) {
        server?.terminateSupervisedChildOnQuit()
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
            guard let updater else {
                NSLog("TcrBar: no updater yet — the check is queued until launch completes")
                checkIsPending = true
                return
            }
            updater.checkForUpdates()
        default:
            NSLog("TcrBar: unhandled tcrbar URL: %@", url.absoluteString)
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
