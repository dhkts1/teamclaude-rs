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

    var body: some Scene {
        MenuBarExtra {
            FleetView(poller: poller, server: server, loginItem: loginItem, accounts: accounts)
                .onAppear {
                    poller.start()
                    delegate.server = server
                    // macOS owns the login-item bit; re-read it every time the
                    // panel opens so a revocation in System Settings shows up.
                    loginItem.refresh()
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
