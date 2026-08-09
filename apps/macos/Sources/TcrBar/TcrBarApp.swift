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
        // First, and the only one of the three that needs no AppKit at all: it
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
    /// Owned here, not by the panel: the panel is destroyed and rebuilt every
    /// time the menu closes, and an assertion that ended when the menu closed
    /// would be a keep-awake control that keeps nothing awake.
    @StateObject private var awake = AwakeController()

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
                awake: awake,
                startServerAtLaunch: $startServerAtLaunch
            )
            .onAppear {
                poller.start()
                delegate.server = server
                delegate.awake = awake
                // macOS owns the login-item bit; re-read it every time the
                // panel opens so a revocation in System Settings shows up.
                loginItem.refresh()
                if startServerAtLaunch, !didAttemptLaunchStart {
                    didAttemptLaunchStart = true
                    server.start()
                }
            }
        } label: {
            MenuBarLabel(state: poller.state, awake: awake)
        }
        .menuBarExtraStyle(.window)
    }
}

/// Exists so that nothing TcrBar started outlives it — a child process, and a
/// power assertion.
///
/// `terminateSupervisedChildOnQuit()` is a no-op unless *this app* spawned the
/// server — an incumbent proxy is never signalled.
///
/// Both references are handed over from `FleetView.onAppear`, which first runs
/// when the panel is opened. Late, but never too late for either: a server can
/// only be started from that panel or from `startServerAtLaunch` (whose spawn
/// happens in the same `onAppear`), and the keep-awake activity can only be
/// started from that panel too. A `nil` here therefore means there is nothing
/// to release.
@MainActor
final class AppDelegate: NSObject, NSApplicationDelegate {
    var server: ServerController?
    var awake: AwakeController?

    func applicationWillTerminate(_ notification: Notification) {
        server?.terminateSupervisedChildOnQuit()
        // The kernel drops every power assertion a process holds when it dies,
        // so this line is not what lets the Mac sleep again. It is here so that
        // "quitting TcrBar releases it" is something this app does rather than
        // something it gets away with.
        awake?.releaseOnQuit()
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
///
/// Keep-awake is a SECOND, independent mark beside it, and the two never merge.
/// The gauge answers "can I work right now"; keep-awake answers "will this Mac
/// stay up while I do", and recolouring the gauge to say the second thing would
/// destroy the first.
///
/// The mode is signalled on two channels at once, deliberately:
///
///  - **Shape** — an extra glyph that is either there or not. This is the
///    primary channel, because it is the only one that cannot be taken away: it
///    survives greyscale, a red-green colour vision deficiency, and a status
///    item that decides to template the image after all. `Tokens.swift` records
///    this project already rejecting a palette where two states differed in hue
///    alone.
///  - **Colour** — what was actually asked for, and a genuine improvement when
///    it works. It needs a non-template `NSImage`; see `KeepAwakeGlyph` for
///    what was measured about that and what was not.
struct MenuBarLabel: View {
    let state: PollState
    @ObservedObject var awake: AwakeController

    var body: some View {
        HStack(spacing: Tok.space1) {
            capacityGauge
            if awake.isOn { keepAwakeMark }
        }
    }

    @ViewBuilder
    private var capacityGauge: some View {
        switch state {
        case .loaded(let fleet):
            Image(systemName: Tok.glyph(for: fleet.capacityGlyphState))
        case .pending:
            Image(systemName: "gauge.with.dots.needle.33percent")
        case .toolMissing, .commandFailed, .undecodable:
            Image(systemName: Tok.unreadableGlyph)
        }
    }

    /// Falls back to the plain template symbol if the tinted image cannot be
    /// built. Losing the tint costs one channel; drawing nothing would cost the
    /// signal, and the shape is the channel that matters.
    @ViewBuilder
    private var keepAwakeMark: some View {
        if let tinted = KeepAwakeGlyph.image(tint: Tok.awakeNSColor) {
            Image(nsImage: tinted)
                .accessibilityLabel(KeepAwakeGlyph.accessibilityDescription)
        } else {
            Image(systemName: KeepAwakeGlyph.symbolName)
                .accessibilityLabel(KeepAwakeGlyph.accessibilityDescription)
        }
    }
}
