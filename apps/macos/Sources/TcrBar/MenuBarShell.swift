import AppKit
import Combine
import SwiftUI
import TcrBarCore

/// The menu bar item and the panel that hangs off it, owned by this app rather
/// than by SwiftUI.
///
/// ## Why it is hand-managed
///
/// A `MenuBarExtra` renders its label monochrome whatever the image says —
/// measured across six label constructions, table in ``MenuBarMark``. The only
/// construction that carries an arbitrary colour is setting `button.image` on the
/// real `NSStatusBarButton`, and a SwiftUI scene never lets you near it. Owning
/// the status item is what makes the keep-awake mark able to be cyan.
///
/// `FleetView` is unchanged. What changed is who owns the window it lives in: an
/// `NSPopover` with one long-lived `NSHostingController` instead of a panel that
/// `MenuBarExtra` destroyed and rebuilt on every open. Three things came free
/// with that rebuild-per-open and now have to be arranged for, each noted at the
/// line that arranges it: the login-item re-read, the panel's size, and key
/// focus.
@MainActor
final class MenuBarShell {
    let poller: StatusPoller
    let server: ServerController
    let loginItem: LoginItem
    let accounts: AccountController
    /// Owned here, not by the panel: the panel is a view that can be torn down,
    /// and an assertion that ended when the panel closed would be a keep-awake
    /// control that keeps nothing awake.
    let awake: AwakeController
    let preference: LaunchPreference

    let statusItem: NSStatusItem
    let popover: NSPopover

    private var marks: Set<AnyCancellable> = []

    /// `nil` means "the real one". A default argument is evaluated in a
    /// *nonisolated* context, and every controller here is `@MainActor`, so
    /// `poller: StatusPoller = StatusPoller()` does not compile — the optionals
    /// are what let the probe substitute a pinned poller and an inert keep-awake
    /// while the app passes nothing at all.
    init(
        poller: StatusPoller? = nil,
        server: ServerController? = nil,
        loginItem: LoginItem? = nil,
        accounts: AccountController? = nil,
        awake: AwakeController? = nil,
        preference: LaunchPreference? = nil
    ) {
        self.poller = poller ?? StatusPoller()
        self.server = server ?? ServerController()
        self.loginItem = loginItem ?? LoginItem()
        self.accounts = accounts ?? AccountController()
        self.awake = awake ?? AwakeController()
        self.preference = preference ?? LaunchPreference()

        statusItem = NSStatusBar.system.statusItem(withLength: NSStatusItem.variableLength)
        // An `NSStatusItem`'s visibility is *persisted*, and the app must never
        // depend on the persisted value.
        //
        // AppKit stores it per status item in this app's defaults domain, under
        // `"NSStatusItem VisibleCC Item-0"`. A single ⌘-drag of the icon out of
        // the menu bar writes `0` there, and every status item this app creates
        // afterwards is born hidden — permanently, silently, and across
        // reinstalls, because the value outlives the binary. The symptom is not
        // a crash or a log line: the app launches, polls, holds its assertions
        // and draws nothing a human can see.
        //
        // Both `com.github.dhkts1.tcrbar` (the bundled app) and `TcrBar` (what
        // an unbundled `swift build` binary uses) were observed holding `0`.
        // How it got there is *not* established — it could predate this work or
        // have been written during it — so nothing here claims a history, and
        // nothing here claims a current value either: AppKit writes the key
        // back on every run, so it was measured flipping to `1` and back within
        // minutes. That is the whole argument for this line. Setting it
        // explicitly at creation is correct under every history and every
        // stored value, which is the point: the shipped behaviour must not be a
        // function of what is in `defaults`.
        //
        // No `autosaveName`. It would only move the same persisted flag to a
        // differently-named key with the identical failure mode; the
        // unconditional assignment below is what makes the stored value
        // irrelevant, and `--shell-probe` assertion 1 asserts the result.
        statusItem.isVisible = true
        popover = NSPopover()

        let hosting = NSHostingController(
            rootView: FleetPanel(
                poller: self.poller, server: self.server, loginItem: self.loginItem,
                accounts: self.accounts, awake: self.awake, preference: self.preference))
        // Without this the popover takes a default size and the panel is clipped.
        //
        // This is the specific thing `MenuBarExtra` did for free. `FleetView`
        // measures its own row height through a `GeometryReader` preference
        // (`FleetView.swift:44-56, 178-189`), which exists precisely because a
        // scroll view's ideal height collapses to about one row — so a shell
        // that does not propagate the preferred size up to the popover
        // reproduces that exact bug, and it looks like a SwiftUI layout problem
        // rather than a missing line here. `--shell-probe` assertion 5 checks
        // the resulting `contentSize` numerically.
        hosting.sizingOptions = [.preferredContentSize]
        popover.contentViewController = hosting
        // What restores click-outside dismissal, which a menu had by nature.
        popover.behavior = .transient

        if let button = statusItem.button {
            button.target = self
            button.action = #selector(togglePanel(_:))
        }

        // Both publishers, combined, so the image is recomposed whenever either
        // half of it changes.
        //
        // The values come from the publisher, never from re-reading the
        // controllers. `@Published` fires in `willSet`, so `awake.isOn` inside
        // this sink is still the OLD value — a mark composed from it would
        // disagree with `AwakeController.isOn` for one edge in each direction,
        // which is the same "three representations of one fact" failure that
        // controller's own doc-comment is built to prevent.
        self.poller.$state
            .combineLatest(self.awake.$isOn)
            .sink { [weak self] state, isOn in
                self?.updateMark(state: state, awake: isOn)
            }
            .store(in: &marks)
    }

    // MARK: - The mark

    /// Which gauge the capacity state draws. Fleet *capacity*, not the worst
    /// account: in a rotating pool spent accounts are the mechanism working, so a
    /// worst-wins glyph pinned itself to the alarm state whenever any one of
    /// thirteen accounts was spent, which is nearly always. The mapping itself
    /// lives in `Fleet.capacityGlyphState`; there is no logic here. A failed read
    /// shows a warning glyph rather than a healthy-looking gauge.
    static func gaugeSymbol(for state: PollState) -> String {
        switch state {
        case .loaded(let fleet):
            return Tok.glyph(for: fleet.capacityGlyphState)
        case .pending:
            return "gauge.with.dots.needle.33percent"
        case .toolMissing, .commandFailed, .undecodable:
            return Tok.unreadableGlyph
        }
    }

    /// One line for the tooltip. The menu bar has room for a glyph and nothing
    /// else, so this is where the poll's own summary is reachable by a human who
    /// has not opened the panel.
    static func toolTip(state: PollState, awake: Bool) -> String {
        awake ? "\(state.summary) · \(KeepAwakeGlyph.accessibilityDescription)" : state.summary
    }

    private func updateMark(state: PollState, awake isOn: Bool) {
        guard let button = statusItem.button else { return }
        if let mark = MenuBarMark.image(
            gaugeSymbol: Self.gaugeSymbol(for: state), awake: isOn,
            awakeTint: Tok.awakeNSColor)
        {
            button.image = mark
            button.title = ""
        } else if button.image == nil {
            // Only reachable if an SF Symbol this build names has gone missing.
            // A status item with neither image nor title is zero points wide and
            // invisible, which reads as "the app did not launch" — so say
            // something rather than disappear.
            button.title = "tcr"
        }
        button.toolTip = Self.toolTip(state: state, awake: isOn)
    }

    // MARK: - The panel

    @objc private func togglePanel(_ sender: Any?) {
        if popover.isShown { closePanel() } else { openPanel() }
    }

    func openPanel() {
        guard let button = statusItem.button else { return }
        // macOS owns the login-item bit and the operator can revoke it in System
        // Settings, so a cached value is a lie (`LoginItem.swift:5-12`). Under
        // `MenuBarExtra` this rode on `FleetView`'s own `.onAppear`, which fired
        // on every open because the panel was rebuilt every time. One popover
        // keeps one hosting controller for the life of the app, so that
        // `onAppear` now fires once and never again — losing this line is a
        // silent regression, not a visible one.
        loginItem.refresh()
        // Without activation the panel opens without key focus, and
        // `.textSelection(.enabled)` on the account name (`FleetView.swift:537`)
        // stops working.
        if #available(macOS 14.0, *) {
            NSApp.activate()
        } else {
            NSApp.activate(ignoringOtherApps: true)
        }
        popover.show(relativeTo: button.bounds, of: button, preferredEdge: .minY)
    }

    func closePanel() {
        popover.performClose(nil)
    }
}

/// Hosts `FleetView` and exists for one reason: to turn ``LaunchPreference`` into
/// the `Binding<Bool>` that view already takes.
///
/// `FleetView` is deliberately untouched by the shell rewrite. Observing the
/// preference *here* is what makes the checkbox move when it is clicked — a
/// binding built straight over `UserDefaults` reads and writes correctly and
/// publishes nothing, so the control would appear stuck.
struct FleetPanel: View {
    @ObservedObject var poller: StatusPoller
    @ObservedObject var server: ServerController
    @ObservedObject var loginItem: LoginItem
    @ObservedObject var accounts: AccountController
    @ObservedObject var awake: AwakeController
    @ObservedObject var preference: LaunchPreference

    var body: some View {
        FleetView(
            poller: poller,
            server: server,
            loginItem: loginItem,
            accounts: accounts,
            awake: awake,
            startServerAtLaunch: $preference.startServerAtLaunch
        )
    }
}
