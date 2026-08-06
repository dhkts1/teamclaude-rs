import SwiftUI
import TcrBarCore

/// TcrBar — a menu-bar accessory for the `tcr` rotating proxy.
///
/// `LSUIElement` is set in the bundle's Info.plist, so there is no Dock icon and
/// no main window: the whole app is the `MenuBarExtra`.
@main
struct TcrBarApp: App {
    @NSApplicationDelegateAdaptor(AppDelegate.self) private var delegate
    @StateObject private var poller = StatusPoller()
    @StateObject private var server = ServerController()
    @StateObject private var loginItem = LoginItem()
    @StateObject private var accounts = AccountController()

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
        }
        .menuBarExtraStyle(.window)
    }
}

/// Exists for one reason: a child process TcrBar spawned must not outlive it.
///
/// `terminateSupervisedChildOnQuit()` is a no-op unless *this app* spawned the
/// server — an incumbent proxy is never signalled.
@MainActor
final class AppDelegate: NSObject, NSApplicationDelegate {
    var server: ServerController?

    func applicationWillTerminate(_ notification: Notification) {
        server?.terminateSupervisedChildOnQuit()
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
