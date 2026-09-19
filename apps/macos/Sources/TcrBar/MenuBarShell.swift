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
    /// The identity-bound control account, read via `tcr control --show` and
    /// set/cleared via `tcr control`. Owned here, alongside `accounts`, for the
    /// same reason: it outlives the panel view, and `openPanel()` refreshes it
    /// the same way it refreshes `loginItem`.
    let control: ControlAccountController
    /// Owned here, not by the panel: the panel is a view that can be torn down,
    /// and an assertion that ended when the panel closed would be a keep-awake
    /// control that keeps nothing awake.
    let awake: AwakeController
    /// Macs waiting on an answer, read with the panel CLOSED.
    ///
    /// Owned here for the reason every other controller on this object is: the
    /// Peers tab's own `PeerController` starts on `onAppear` and stops on
    /// `onDisappear`, so with the panel shut nothing in this app knows a knock
    /// exists. The panel is shut almost always, which is the whole case the
    /// menu-bar mark and the notification are for.
    let knocks: KnockReader
    let preference: LaunchPreference
    /// "Show counts in the menu bar" — whether ``updateMark`` draws the
    /// `ready/enabled` label. Owned here for the same reason as `preference`:
    /// the mark it gates is composed on every poll tick, not just while the
    /// panel is open.
    let countsPreference: MenuBarCountsPreference
    /// Group-membership mutations for the Groups view, owned here for the
    /// same reason as `accounts`: an in-flight/failure/restart-notice state
    /// that reset every time the panel opened would lose the "restart the
    /// proxy to apply" note the moment the operator closed it.
    let groupController: GroupController
    /// Account deletion for the gear menu's "Delete Account…" action, owned
    /// here for the same reason as `groupController`: the "restart the proxy
    /// to apply" notice must survive the panel closing and reopening, not
    /// reset the moment the operator dismisses it.
    let removeController: RemoveAccountController
    /// Owned here for the same reason as the rest, plus one of its own: the
    /// delegate's `tcrbar://check-for-updates` handler reaches through the shell
    /// to find it, so an updater that only existed while the panel was open would
    /// make the CLI's call do nothing on an app nobody had clicked.
    let updater: Updater
    /// The release-notes window and its state. Owned here so the notes loaded
    /// at launch are the ones the footer button reopens, and so the
    /// right-click menu can reach it with the panel closed.
    let whatsNew: WhatsNewController
    let whatsNewWindow: WhatsNewWindow
    /// "Show the running-tool count" and "Open on" — the two Settings-window
    /// rows with no pre-existing controller (Menu Bar pane). Owned here for the same
    /// reason as `countsPreference`: read by every poll tick, not just while
    /// the Settings window happens to be open.
    let runningToolsPreference: ShowRunningToolsPreference
    let defaultTabPreference: DefaultTabPreference
    /// "Panel density" (Settings window, Menu Bar pane). Owned here, same
    /// shape as `defaultTabPreference`, even though `V4.compact` never reads
    /// this instance — the picker binding in `SettingsPanes` needs a live
    /// `ObservedObject` to write through, and one built per Settings-window
    /// open would lose the picker's selection to the window's own rebuild.
    let panelDensityPreference: PanelDensityPreference
    /// The Settings window itself. Lazy and reused, same shape as
    /// `whatsNewWindow` — a window rebuilt per open loses its sidebar
    /// selection and its size.
    private(set) lazy var settingsWindow = SettingsWindowController(
        dependencies: SettingsDependencies(
            poller: poller, server: server, loginItem: loginItem, awake: awake,
            preference: preference, countsPreference: countsPreference,
            runningToolsPreference: runningToolsPreference,
            defaultTabPreference: defaultTabPreference,
            panelDensityPreference: panelDensityPreference,
            groupController: groupController, updater: updater,
            onWhatsNew: { [weak self] in self?.openWhatsNew() }))

    let statusItem: NSStatusItem
    let popover: NSPopover

    private var marks: Set<AnyCancellable> = []

    /// `nil` means "the real one". A default argument is evaluated in a
    /// *nonisolated* context, and every controller here is `@MainActor`, so
    /// `poller: StatusPoller = StatusPoller()` does not compile — the optionals
    /// are what let the probe substitute a pinned poller, an inert keep-awake and
    /// an unstarted updater while the app passes nothing at all.
    init(
        poller: StatusPoller? = nil,
        server: ServerController? = nil,
        loginItem: LoginItem? = nil,
        accounts: AccountController? = nil,
        control: ControlAccountController? = nil,
        awake: AwakeController? = nil,
        preference: LaunchPreference? = nil,
        countsPreference: MenuBarCountsPreference? = nil,
        updater: Updater? = nil,
        groupController: GroupController? = nil,
        removeController: RemoveAccountController? = nil,
        whatsNew: WhatsNewController? = nil,
        runningToolsPreference: ShowRunningToolsPreference? = nil,
        defaultTabPreference: DefaultTabPreference? = nil,
        panelDensityPreference: PanelDensityPreference? = nil,
        knocks: KnockReader? = nil
    ) {
        self.poller = poller ?? StatusPoller()
        self.server = server ?? ServerController()
        self.loginItem = loginItem ?? LoginItem()
        self.accounts = accounts ?? AccountController()
        self.control = control ?? ControlAccountController()
        self.awake = awake ?? AwakeController()
        // Never started here: `AppDelegate` starts it beside the poller, and
        // `--shell-probe` builds this object without ever spawning a `tcr`.
        self.knocks = knocks ?? KnockReader()
        self.preference = preference ?? LaunchPreference()
        self.countsPreference = countsPreference ?? MenuBarCountsPreference()
        self.updater = updater ?? Updater()
        self.groupController = groupController ?? GroupController()
        self.removeController = removeController ?? RemoveAccountController()
        // The controller's `init` is inert; nothing fetches until `AppDelegate`
        // calls `checkAfterLaunch()`, which the shell probe never does.
        self.whatsNew =
            whatsNew
            ?? WhatsNewController(
                store: WhatsNewStore(),
                fetcher: GitHubReleaseClient(version: AppBuild.shortVersion),
                location: ReleaseFeedLocation.from(bundle: .main),
                currentVersion: AppBuild.shortVersion)
        self.whatsNewWindow = WhatsNewWindow(controller: self.whatsNew)
        self.runningToolsPreference = runningToolsPreference ?? ShowRunningToolsPreference()
        self.defaultTabPreference = defaultTabPreference ?? DefaultTabPreference()
        self.panelDensityPreference = panelDensityPreference ?? PanelDensityPreference()

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
        // Both `io.github.dhkts1.tcrbar` (the bundled app) and `TcrBar` (what
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
                accounts: self.accounts, control: self.control, awake: self.awake,
                preference: self.preference, updater: self.updater,
                groupController: self.groupController, removeController: self.removeController,
                onWhatsNew: { [weak self] in self?.openWhatsNew() },
                onSettings: { [weak self] in self?.openSettings() },
                initialTab: Self.initialTab(from: self.defaultTabPreference)))
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
        // The line above sizes the popover; it is also one half of a layout
        // cycle that aborts the app. The other half is safe-area.
        // `NSHostingView` observes its own frame through KVO, and every frame
        // change runs `invalidateSafeAreaInsets()`, which requests another
        // SwiftUI update, which marks the window as needing another
        // update-constraints pass. The popover resizes itself from those
        // constraints, the hosting view's frame changes again, and AppKit
        // throws from `_postWindowNeedsUpdateConstraints` once the pass count
        // passes its guard. Nothing catches that `NSException`, so `abort()`.
        //
        // Three crash reports from a 0.2.43 build (2026-09-09, macOS 26.6.2,
        // all three throwing at that same frame) show the cycle with no TcrBar
        // frame anywhere in it: it runs entirely between `_NSPopoverWindow`,
        // `NSPopoverFrame` and `NSHostingView`, driven from `stepIdle` with no
        // user event in the stack.
        //
        // `PanelHeight.settled` damps the OTHER loop, the one that runs
        // through `onPreferenceChange`. Its predecessor `PanelHeight.quantized`
        // shipped in that same 0.2.43 build, which is the evidence that damping
        // that loop is not sufficient alone: preference quantization cannot
        // reach a cycle that never reads a preference, and none of the three
        // 0.2.43 stacks contains a TcrBar frame at all.
        //
        // `quantized` is no longer called from this target — `settled` owns the
        // publish path and calls it internally, and it was made internal to
        // `TcrBarCore` so that re-adding it at a `GeometryReader` emitter fails
        // to compile rather than silently reverting the fix.
        //
        // A popover has no safe area to inset against: no notch, no title bar,
        // no keyboard. Opting out is correct on its own terms, and it is the
        // edge of the cycle that can be cut without giving up the preferred
        // size that assertion 5 checks.
        //
        // Not reproduced locally: the machine that crashes is one point
        // release ahead (26.6.2 vs 26.6.1) and the panel is stable here across
        // days. This cuts a documented edge of the cycle in the crash stack;
        // it has not been watched to fail and then pass.
        if #available(macOS 13.3, *) {
            hosting.safeAreaRegions = []
        }
        popover.contentViewController = hosting
        // What restores click-outside dismissal, which a menu had by nature.
        popover.behavior = .transient

        if let button = statusItem.button {
            button.target = self
            button.action = #selector(togglePanel(_:))
            // Left click still opens the popover, unchanged. Right click (and
            // Control-click, which AppKit reports as the same `.rightMouseUp`)
            // is the only thing added here — the button otherwise only ever
            // sends on `.leftMouseUp`.
            button.sendAction(on: [.leftMouseUp, .rightMouseUp])
        }

        // All three publishers, combined, so the image and label are
        // recomposed whenever any of them changes.
        //
        // The values come from the publisher, never from re-reading the
        // controllers. `@Published` fires in `willSet`, so `awake.isOn` inside
        // this sink is still the OLD value — a mark composed from it would
        // disagree with `AwakeController.isOn` for one edge in each direction,
        // which is the same "three representations of one fact" failure that
        // controller's own doc-comment is built to prevent. `showCounts`
        // follows the identical rule for the same reason: toggling it and
        // reading `countsPreference.showCounts` back inside this sink would
        // race the very publisher this sink exists to trust.
        // The knock count is combined in here for the same reason the other
        // three are: the mark is recomposed whenever ANY of them changes, and
        // a knock arriving between two polls must move the bar without
        // waiting for an account figure to change.
        self.poller.$state
            .combineLatest(self.awake.$isOn, self.countsPreference.$showCounts)
            .combineLatest(self.runningToolsPreference.$showRunningToolCount)
            .combineLatest(self.knocks.$knocks)
            .sink { [weak self] outer, pending in
                let (combined, showRunningTools) = outer
                let (state, isOn, showCounts) = combined
                self?.updateMark(
                    state: state, awake: isOn, showCounts: showCounts,
                    showRunningTools: showRunningTools, knocks: pending.count)
            }
            .store(in: &marks)

        // Phase 3 of `docs/plans/panel-sizing-generalization.md`: predict the
        // panel's size on every poll tick and log it beside the size SwiftUI
        // actually produced. Its own sink rather than a line inside the one
        // above, because that one exists to compose the menu bar mark and
        // combines a second publisher to do it — a size measurement would then
        // also fire on every keep-awake toggle, which changes nothing about the
        // panel's height and would print a duplicate of the previous line.
        //
        // Nothing here writes anything. `sizingOptions` is untouched, the
        // popover still sizes itself, and the only output is one `NSLog`.
        self.poller.$state
            .sink { [weak self] state in
                self?.logPanelSize(state: state)
            }
            .store(in: &marks)
    }

    // MARK: - The mark

    /// Which colour the one coffee-cup glyph wears. `PollState.capacityTintKind(awake:)`
    /// (`TcrBarCore/StatusPoller.swift`) is the whole decision — testable there,
    /// against no `NSColor` at all — and this is the one place `Tok`'s actual
    /// colours attach to it.
    static func cupTint(for state: PollState, awake: Bool) -> MenuBarMark.Tint {
        switch state.capacityTintKind(awake: awake) {
        case .failed: return .failed(Tok.spentNSColor)
        case .near: return .near(Tok.nearNSColor)
        case .awake: return .awake(Tok.awakeNSColor)
        case .template: return .template
        }
    }

    /// One line for the tooltip. The menu bar has room for a glyph and nothing
    /// else, so this is where the poll's own summary is reachable by a human who
    /// has not opened the panel. ``PollState/tooltipSentence`` is the fuller
    /// capacity sentence when a healthy read has one to give, and
    /// ``PollState/summary`` unchanged for every other case.
    ///
    /// The running-tools clause, when the segment is shown, sits between that
    /// sentence and the keep-awake clause — matching the mockup's own ordering
    /// (`docs/design/menubar-mark-mockup.html`, "… · 2 parked · 3 tools
    /// running"), never colour alone: the amber count on the glyph has no
    /// accessible text of its own, but this sentence already says "near their
    /// limit" in words, which is what "never colour alone" asks for.
    /// A Mac waiting on an answer is PREPENDED, in its own sentence, ahead of
    /// every capacity clause: it is the one thing on this item that is waiting
    /// on a person, and the pointer route is where the amber glyph beside the
    /// cup says what it means in words.
    static func toolTip(
        state: PollState, awake: Bool, showRunningTools: Bool, knocks: Int = 0
    ) -> String {
        var sentence = state.tooltipSentence
        if let running = state.runningToolsCount(showRunningTools: showRunningTools) {
            let noun = running == 1 ? "tool" : "tools"
            sentence += " · \(running) \(noun) running"
        }
        if awake {
            sentence = "\(sentence) · \(KeepAwakeGlyph.accessibilityDescription)"
        }
        guard let asking = PeerAdmission.knockBarSentence(count: knocks) else { return sentence }
        return "\(asking) \(sentence)"
    }

    /// The knock segment: `person.fill.questionmark` in amber, plus the count
    /// once more than one Mac is asking.
    ///
    /// `nil` at zero, which is what keeps an empty title empty.
    ///
    /// The same text-attachment shape the running-tools glyph uses, at the
    /// same 13 pt and the same baseline nudge, because these are the two
    /// glyphs that sit in this one label and a second construction is how they
    /// come to sit at two different heights. The count is NOT drawn at one:
    /// a `1` beside a glyph that is only there when somebody is asking says
    /// nothing the glyph did not.
    static func knockAttributedSegment(count: Int) -> NSAttributedString? {
        guard count > 0 else { return nil }
        let font = NSFont.monospacedDigitSystemFont(
            ofSize: NSFont.menuBarFont(ofSize: 0).pointSize, weight: .regular)
        guard
            let glyph = NSImage(
                systemSymbolName: "person.fill.questionmark",
                accessibilityDescription: PeerAdmission.knockBarSentence(count: count))
        else { return nil }
        glyph.isTemplate = true
        let tinted = NSImage(size: glyph.size, flipped: false) { rect in
            Tok.nearNSColor.set()
            glyph.draw(in: rect, from: .zero, operation: .sourceOver, fraction: 1)
            rect.fill(using: .sourceAtop)
            return true
        }
        let attachment = NSTextAttachment()
        attachment.image = tinted
        attachment.bounds = NSRect(x: 0, y: -2, width: 13, height: 13)
        let result = NSMutableAttributedString(attachment: attachment)
        guard count > 1 else { return result }
        result.append(
            NSAttributedString(
                string: " \(count)",
                attributes: [.font: font, .foregroundColor: Tok.nearNSColor]))
        return result
    }

    /// `PollState.countsLabel`, rendered with tabular figures so the status
    /// item does not jitter in width as the digits change between polls, plus
    /// the running-tools segment (`9/13 · ⌘3`-shaped, mockup's second bar) when
    /// `runningTools` is non-`nil`.
    ///
    /// This label is opt-in (the counts preference, default off): the cup
    /// itself already carries capacity by default, as its fill level and its
    /// own colour (``MenuBarShell/cupTint(for:awake:)``) — this is a second,
    /// numeric rendering of the identical fact for an operator who wants it
    /// spelled out. `amber` matches whatever tint the cup drew for the same
    /// poll, so the two can never disagree about which state they describe.
    /// The separator dot is drawn `Tok.mute` so it reads as
    /// punctuation rather than a second urgency signal, and the running count
    /// itself is `labelColor` — amber marks capacity, not tool activity.
    /// Internal, not `private`: `RenderMark` composes the identical title this
    /// method builds, onto its own canvas rather than a real `NSStatusItem`, so
    /// the render fixtures and the live mark can never draw the label two
    /// different ways.
    static func countsAttributedTitle(
        _ label: String, amber: Bool, runningTools: Int?
    ) -> NSAttributedString {
        let font = NSFont.monospacedDigitSystemFont(
            ofSize: NSFont.menuBarFont(ofSize: 0).pointSize, weight: .regular)
        let result = NSMutableAttributedString(
            string: label,
            attributes: [.font: font, .foregroundColor: amber ? Tok.nearNSColor : .labelColor])
        guard let runningTools else { return result }

        result.append(
            NSAttributedString(
                string: " \u{00b7} ",
                attributes: [.font: font, .foregroundColor: NSColor(Tok.mute)]))
        if let terminal = NSImage(
            systemSymbolName: "terminal", accessibilityDescription: "tools running"
        ) {
            terminal.isTemplate = true
            let attachment = NSTextAttachment()
            attachment.image = terminal
            // 13pt, matching the mockup's second bar ("the terminal glyph at
            // 13 pt"); the small negative y nudges it onto the same baseline
            // as the tabular digits either side of it.
            attachment.bounds = NSRect(x: 0, y: -2, width: 13, height: 13)
            result.append(NSAttributedString(attachment: attachment))
            result.append(NSAttributedString(string: " ", attributes: [.font: font]))
        }
        result.append(
            NSAttributedString(
                string: "\(runningTools)",
                attributes: [.font: font, .foregroundColor: NSColor.labelColor]))
        return result
    }

    /// The status item's title, built in two independent halves.
    ///
    /// The knock segment comes FIRST and is drawn whether or not the counts
    /// preference is on: somebody waiting on an answer is not a preference,
    /// and the `else` branch used to write `button.title = ""` over it. What
    /// stays empty is a title with neither half, which is the default Mac with
    /// counts off and nobody asking.
    ///
    /// The order is fixed: what wants an answer, then what the fleet is doing.
    static func markTitle(
        state: PollState, showCounts: Bool, showRunningTools: Bool, knocks: Int
    ) -> NSAttributedString {
        let title = NSMutableAttributedString()
        if let asking = Self.knockAttributedSegment(count: knocks) {
            title.append(asking)
        }
        // `state.countsLabel` (`TcrBarCore/StatusPoller.swift`) is `nil` for
        // the same cases the cup's fill already carries — pending, a failed
        // read, an all-disabled fleet — so the guard here is purely
        // `showCounts`; the state check already happened.
        guard showCounts, let label = state.countsLabel else { return title }
        if title.length > 0 {
            title.append(NSAttributedString(string: " "))
        }
        title.append(
            Self.countsAttributedTitle(
                label, amber: state.countIsNearCapacity,
                runningTools: state.runningToolsCount(showRunningTools: showRunningTools)))
        return title
    }

    private func updateMark(
        state: PollState, awake isOn: Bool, showCounts: Bool, showRunningTools: Bool,
        knocks: Int
    ) {
        guard let button = statusItem.button else { return }
        if let mark = MenuBarMark.image(
            fraction: state.capacityFraction, tint: Self.cupTint(for: state, awake: isOn),
            knocks: knocks)
        {
            button.image = mark
            button.attributedTitle = Self.markTitle(
                state: state, showCounts: showCounts, showRunningTools: showRunningTools,
                knocks: knocks)
        } else if button.image == nil {
            // Only reachable if an SF Symbol this build names has gone missing.
            // A status item with neither image nor title is zero points wide and
            // invisible, which reads as "the app did not launch" — so say
            // something rather than disappear.
            button.title = "tcr"
        }
        button.toolTip = Self.toolTip(
            state: state, awake: isOn, showRunningTools: showRunningTools, knocks: knocks)
    }

    // MARK: - The panel's size, predicted against what it really is

    /// One line per poll tick: what ``PanelSize`` says the popover should be,
    /// beside what the popover actually is.
    ///
    /// This is the whole of phase 3 and it changes no behaviour. The question
    /// the five-phase plan rests on — does a TextKit estimate taken outside the
    /// view graph track what SwiftUI lays out inside it — could not be answered
    /// by argument in the design review, and cannot be answered on this machine
    /// either: it wants a real fleet on a real Mac, and the one that crashes is
    /// somebody else's. So the app answers it, and the only cost of being wrong
    /// here is a wrong number in a log.
    ///
    /// `state` comes from the publisher and is never re-read off the poller.
    /// `@Published` fires in `willSet`, so `poller.state` inside this call is
    /// still the PREVIOUS poll — a line composed from it would pair one tick's
    /// prediction with the next tick's account count, which is exactly the kind
    /// of quiet mismatch a reader has no way to see. The same rule the mark's
    /// own sink follows, and for the same reason.
    ///
    /// `popover.contentSize` is read here rather than `hosting.view.frame`:
    /// `contentSize` is what phase 4 will assign, so this measures the property
    /// that is about to change owners rather than a proxy for it. With the
    /// panel closed it is whatever the last open left behind, which is why the
    /// line says `shown=no` instead of dropping the reading — a reader who
    /// wants only live layouts can filter, and one debugging a panel that never
    /// opens still gets lines.
    private func logPanelSize(state: PollState) {
        let delta = PanelSizeProbe.delta(
            state: state,
            update: updater.updateState,
            server: server.state,
            actualContentSize: popover.contentSize,
            isPanelShown: popover.isShown)
        // `"%@"` and not the string itself: `NSLog` takes a format string, and
        // a panel line carries `%` in it the moment a cache-hit percentage
        // reaches the header — `TcrBarApp.swift:72` logs through the same guard
        // for the same reason.
        NSLog("%@", delta.logLine)
    }

    // MARK: - The panel

    @objc private func togglePanel(_ sender: Any?) {
        // `NSApp.currentEvent` is how a single action selector, wired to both
        // mouse buttons above, tells them apart — AppKit does not pass the
        // triggering event to the action itself. A right-click (or a
        // Control-click, which arrives as the same `.rightMouseUp`) opens the
        // quick-actions menu instead of the popover; anything else falls
        // through to the original left-click behaviour, unchanged.
        if NSApp.currentEvent?.type == .rightMouseUp {
            showQuickActionsMenu()
            return
        }
        if popover.isShown { closePanel() } else { openPanel() }
    }

    // MARK: - Quick actions

    /// Deliberately never assigned to `statusItem.menu`: doing that makes
    /// AppKit show the menu on *every* click, left included, which is exactly
    /// the popover-breaking regression this feature must not cause.
    /// `NSMenu.popUp(positioning:at:in:)` shows a menu once, transiently, with
    /// the status item's own click handling untouched.
    private func showQuickActionsMenu() {
        guard let button = statusItem.button else { return }
        let menu = NSMenu()

        let serverItem: NSMenuItem
        // `startDisabledReason` is `nil` on every state but a NON-proxy
        // process holding the port (`ServerController+StartServerReason`),
        // read off the same check `server.start()` already ran on its last
        // attempt, not a new port probe. A colleague's "Start server" was
        // disabled with no reason at all; this is the reason.
        if server.state.isOurChild {
            serverItem = NSMenuItem(
                title: "Stop server", action: #selector(quickStopServer), keyEquivalent: "")
            serverItem.target = self
            menu.addItem(serverItem)
        } else if let reason = server.state.startDisabledReason {
            serverItem = NSMenuItem(title: "Start server", action: nil, keyEquivalent: "")
            serverItem.isEnabled = false
            menu.addItem(serverItem)
            let reasonItem = NSMenuItem(title: reason, action: nil, keyEquivalent: "")
            reasonItem.isEnabled = false
            reasonItem.indentationLevel = 1
            menu.addItem(reasonItem)
        } else {
            serverItem = NSMenuItem(
                title: "Start server", action: #selector(quickStartServer), keyEquivalent: "")
            serverItem.target = self
            menu.addItem(serverItem)
        }

        let refreshItem = NSMenuItem(
            title: "Refresh", action: #selector(quickRefresh), keyEquivalent: "")
        refreshItem.target = self
        menu.addItem(refreshItem)

        menu.addItem(.separator())

        let awakeItem = NSMenuItem(
            title: "Keep this Mac awake", action: #selector(quickToggleAwake), keyEquivalent: "")
        awakeItem.target = self
        awakeItem.state = awake.isOn ? .on : .off
        menu.addItem(awakeItem)

        menu.addItem(.separator())

        let updateItem = NSMenuItem(
            title: "Check for Updates…", action: #selector(quickCheckForUpdates),
            keyEquivalent: "")
        updateItem.target = self
        updateItem.isEnabled = updater.canCheckForUpdates
        menu.addItem(updateItem)

        let notesItem = NSMenuItem(
            title: "What's New…", action: #selector(quickWhatsNew), keyEquivalent: "")
        notesItem.target = self
        menu.addItem(notesItem)

        let quitItem = NSMenuItem(
            title: "Quit", action: #selector(quickQuit), keyEquivalent: "")
        quitItem.target = self
        menu.addItem(quitItem)

        menu.popUp(
            positioning: nil, at: NSPoint(x: 0, y: button.bounds.height + 4), in: button)
    }

    @objc private func quickStartServer() { server.start() }
    @objc private func quickStopServer() { server.stop() }
    @objc private func quickRefresh() { Task { await poller.pollOnce() } }
    @objc private func quickToggleAwake() { awake.toggle() }
    @objc private func quickCheckForUpdates() { updater.checkForUpdates() }
    @objc private func quickWhatsNew() { openWhatsNew() }

    /// The on-demand route: footer button, right-click item. Shows the window
    /// at once (loading state) and lets the controller fill it in.
    func openWhatsNew() {
        closePanel()
        whatsNewWindow.present()
        Task { await whatsNew.showOnDemand() }
    }
    @objc private func quickQuit() { NSApplication.shared.terminate(nil) }

    /// The gear button's action (`⌘,`, every tab header). Closes the popover
    /// first — the same reasoning `openWhatsNew()` already follows — since a
    /// transient popover would otherwise dismiss itself the instant the
    /// Settings window steals key focus.
    func openSettings() {
        closePanel()
        settingsWindow.show()
    }

    /// `DefaultTabPreference` stores a plain string (see that type's own
    /// doc-comment on why it cannot hold `PanelTab` directly); this is the
    /// one place that turns it back into the real enum, at the one call site
    /// that constructs the live panel.
    ///
    /// `PanelTab(rawValue:)` rather than a `switch` over the names. The switch
    /// listed three of the four and sent everything else to `.accounts`, so a
    /// stored `peers`, which ``DefaultTabPreference`` accepts and the picker
    /// now offers, was read, validated, and then silently opened the Accounts
    /// tab. A value this enum does not have still falls back, which is the
    /// same treatment ``DefaultTabPreference`` gives a hand-edited key.
    static func initialTab(from preference: DefaultTabPreference) -> PanelTab {
        PanelTab(rawValue: preference.tab) ?? .accounts
    }

    func openPanel() {
        guard let button = statusItem.button else { return }
        // Without activation the panel opens without key focus, and
        // `.textSelection(.enabled)` on the account name (`FleetView.swift:537`)
        // stops working.
        //
        // `ignoringOtherApps: true`, the same as `WhatsNewWindow.present()` and
        // `Updater.checkForUpdates()`. This used to take the cooperative
        // `NSApp.activate()` on macOS 14+, on the reasoning — written down in
        // `WhatsNewWindow` — that cooperative activation is enough here because
        // a click opened the popover. That is the assumption this call got wrong.
        //
        // Cooperative activation only succeeds while the system still credits
        // this app with a recent interaction, and the status-item click does not
        // reliably earn that once another app holds activation. Measured on a
        // stuck panel: popover OPEN, `ApplicationType=UIElement` (so the policy
        // was fine), and the frontmost app was a different one entirely. The
        // popover draws but never becomes key, so every control in it is dead
        // while `.transient`'s own monitors keep Escape and click-outside
        // working — which is what made it look like a rendering bug rather than
        // a focus one.
        //
        // Re-login is the reliable way in: it hands focus to the browser, the
        // transient popover closes itself, and the reopen afterwards is the one
        // that cannot take focus back.
        //
        // Deprecated on macOS 14+ and used deliberately anyway: the cooperative
        // replacement has no way to express "the user asked for this window".
        NSApp.activate(ignoringOtherApps: true)
        popover.show(relativeTo: button.bounds, of: button, preferredEdge: .minY)
        // Every re-read the panel needs, started AFTER it is on screen.
        //
        // The panel draws the last snapshot immediately and each of these
        // updates it when it lands. They used to run in front of the show,
        // which put a system call and a subprocess between the click and the
        // first frame for facts the first frame does not need: measured on one
        // machine, the login-item read alone is 19ms to 24ms of blocking main
        // thread, and the open was the only thing waiting for it.
        //
        // macOS owns the login-item bit and the operator can revoke it in
        // System Settings, so a cached value is a lie (`LoginItem.swift:5-12`).
        // Under `MenuBarExtra` this rode on `FleetView`'s own `.onAppear`,
        // which fired on every open because the panel was rebuilt every time.
        // One popover keeps one hosting controller for the life of the app, so
        // that `onAppear` now fires once and never again: losing this line is a
        // silent regression, not a visible one.
        loginItem.refresh()
        // Same reasoning as `loginItem.refresh()` above: another `tcr control`
        // call, from this app's own menu on a previous open, from the CLI
        // directly, or from a second TcrBar instance, can have changed it since
        // this panel last drew, and there is no push channel that would tell
        // this view. `control` is `@Published`, so a stale in-flight open still
        // redraws once this completes.
        Task { await control.refresh() }
        // The fleet, on the same terms, and this one is what the panel is
        // mostly made of.
        //
        // The poll runs on a 3s timer and an open used to take whatever the
        // last tick left, so the figures a person reads after clicking were up
        // to one whole interval old and the fresh ones arrived up to 3000ms
        // later, with nothing on screen saying so. One poll costs a fraction of
        // that: measured on one machine, 229ms, 288ms and 249ms before the two
        // halves were made to run at once. So the open asks for a read of its
        // own rather than waiting for the timer, and the panel it is drawing
        // meanwhile is the same panel it always drew.
        Task { await poller.pollOnce() }
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
    @ObservedObject var control: ControlAccountController
    @ObservedObject var awake: AwakeController
    @ObservedObject var preference: LaunchPreference
    @ObservedObject var updater: Updater
    @ObservedObject var groupController: GroupController
    @ObservedObject var removeController: RemoveAccountController
    var onWhatsNew: () -> Void = {}
    var onSettings: () -> Void = {}
    var initialTab: PanelTab = .accounts

    var body: some View {
        FleetView(
            poller: poller,
            server: server,
            loginItem: loginItem,
            accounts: accounts,
            control: control,
            awake: awake,
            updater: updater,
            groupController: groupController,
            removeController: removeController,
            startServerAtLaunch: $preference.startServerAtLaunch,
            onWhatsNew: onWhatsNew,
            onSettings: onSettings,
            initialTab: initialTab
        )
    }
}
