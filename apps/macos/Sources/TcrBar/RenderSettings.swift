import AppKit
import SwiftUI
import TcrBarCore

/// Rasterise the Settings window, one PNG per pane, in-process, then exit.
///
/// ## Two rendering approaches tried and rejected before this one
///
/// **`ImageRenderer` on the whole `NavigationSplitView`** (attempt 1):
/// every pane past the first came back as AppKit's generic "cannot display
/// this content" glyph — a yellow circle-slash, not a rendering failure with
/// an error on stderr, so `written == attempted` reported a false 8/8.
/// `NavigationSplitView` plus a `.toolbar` needs a real `NSWindow`/`NSToolbar`
/// host, which `ImageRenderer` never provides.
///
/// **`ImageRenderer` on each pane's bare `Form`, no split view** (attempt 2):
/// fixed the placeholder, but produced a pixel-uniform blank/white PNG —
/// `Form(.grouped)` is `NSTableView`-backed, and `ImageRenderer`'s headless
/// layout pass never walks a table view's real `draw(_:)` path either
/// (`FleetView`'s own plain `ScrollView`+`VStack` DOES rasterise fine via
/// `ImageRenderer` — `RenderStates` already proves it — so this is specific
/// to table/list-backed SwiftUI, not scroll content generally). A follow-up
/// fix hosted the bare pane in an OFF-SCREEN `NSWindow` and captured with
/// `NSView.cacheDisplay(in:to:)`: the pane's own text and `Tok`-drawn badges
/// rendered, but the window never got real AppKit compositing (off-screen,
/// never key, never main) — dark captures showed dark-tinted TEXT on a
/// LIGHT background, which is not merely "wrong chrome colour", it is text
/// that is barely legible against its own background. Read, not assumed: a
/// second reviewer caught this by reading `groupsRotation-dark.png` and
/// finding badges and toggle knobs floating on blank white with no row
/// labels, no section cards and no sidebar at all.
///
/// **This version: the REAL `SettingsRootView` (sidebar, toolbar, detail —
/// everything `SettingsWindowController` shows) in a real window, positioned
/// ON a connected screen and given one real display cycle before capture.**
/// The window is `.orderFrontRegardless()`ed rather than skipped, because
/// that ordering — plus a real screen origin — is what makes AppKit's
/// vibrancy/table-view backing stores actually get created; an off-screen
/// origin was tried first and left the same blank/mistinted result attempt
/// 2 already describes. `NSApp.setActivationPolicy` is never touched and no
/// status item or server is ever created (this whole harness runs before
/// `AppDelegate` exists — see `TcrBarEntry.main()`), so this never registers
/// as the running menu-bar app or starts a proxy; the window is closed again
/// immediately after each capture.
///
/// ## Usage
///
///     TcrBar.app/Contents/MacOS/TcrBar --render-settings <output-directory>
///
/// Writes one PNG per pane per appearance and exits without ever showing a
/// menu-bar item, polling `tcr`, or touching a server.
enum RenderSettings {
    static let flag = "--render-settings"

    static func requestedDirectory(_ arguments: [String] = CommandLine.arguments) -> URL? {
        guard let i = arguments.firstIndex(of: flag), i + 1 < arguments.count else { return nil }
        return URL(fileURLWithPath: arguments[i + 1])
    }

    enum Appearance: String, CaseIterable {
        case dark, light
        var nsAppearance: NSAppearance? {
            NSAppearance(named: self == .dark ? .darkAqua : .aqua)
        }
    }

    /// A sheet the Peers pane is asked to open before the capture.
    ///
    /// The mockup's scenes 62 and 63 are sheets, and a sheet is `@State`
    /// behind a press: nothing in a render run can click. The pane already
    /// reaches for this type to decide it must not poll
    /// (`PeersSettingsPane.init`), so the request is read the same way rather
    /// than through a new parameter threaded down `SettingsRootView`.
    ///
    /// The REAL presentation path, not the sheet's body drawn on its own: a
    /// sheet body hosted in a window of its own would be a picture of a view,
    /// not of the sheet an operator gets, and `Form(.grouped)` inside it is
    /// exactly what the two rejected approaches in this file's header could
    /// not draw.
    enum SheetScene: String, CaseIterable {
        /// Scene 62, behind `Customize…`.
        case defaults = "peers-defaults-sheet"
        /// Scene 63, behind a trusted Mac's row.
        case mac = "peers-mac-sheet"
        /// The lease sheet, which `Add a lease…` opens on top of
        /// scene 63. A sheet over a sheet, so the capture walks to the
        /// DEEPEST attached one.
        case lease = "peers-lease-sheet"
    }

    /// Set for the duration of one capture, read by the pane's `onAppear`.
    ///
    /// A static because the pane is built by `SettingsRootView` through
    /// `SettingsTab.allCases` and there is no seam to pass a value through.
    /// Cleared by the same call that sets it, so one scene cannot leak into
    /// the next.
    @MainActor static var requestedSheet: SheetScene?

    @MainActor
    static func run(into directory: URL) -> Never {
        do {
            try FileManager.default.createDirectory(
                at: directory, withIntermediateDirectories: true)
        } catch {
            FileHandle.standardError.write(
                Data("cannot create \(directory.path): \(error)\n".utf8))
            exit(1)
        }

        var written = 0
        var attempted = 0
        for tab in SettingsTab.allCases {
            for appearance in Appearance.allCases {
                attempted += 1
                if render(tab, appearance: appearance, into: directory) { written += 1 }
            }
        }
        // The two sheet scenes, after the panes: same window, same appearance
        // loop, with the pane asked to present one before the capture.
        for scene in SheetScene.allCases {
            for appearance in Appearance.allCases {
                attempted += 1
                if render(.peers, appearance: appearance, sheet: scene, into: directory) {
                    written += 1
                }
            }
        }

        print("\nrendered \(written)/\(attempted) images into \(directory.path)")
        exit(written == attempted ? 0 : 1)
    }

    /// Two groups — one parked, one not — so the Groups & Rotation pane shows
    /// both a real member count and a real parked toggle state, the same way
    /// `RenderStates`'s own fixtures avoid an all-healthy fleet that cannot
    /// distinguish "wired correctly" from "never wired at all".
    private static func fixtureAccounts() -> [Account] {
        [
            fixtureAccount("alice@example.com", groups: ["dev"], groupColors: ["dev": "#0a84ff"]),
            fixtureAccount("bob@example.com", groups: ["dev"], groupColors: ["dev": "#0a84ff"]),
            fixtureAccount(
                "carol@example.com", groups: ["henry-team"], parkedGroups: ["henry-team"],
                groupColors: ["henry-team": "#32d74b"]),
        ]
    }

    /// 660×581, the reviewed window as a FRAME
    /// (`docs/design/panel-tabs-review.md`: "window 660×581 with a 200px
    /// sidebar"), which is `SettingsWindowController`'s 540 pt of content plus
    /// its title bar. Used as a content size here, so these four panes are
    /// captured a little roomier than they open. Peers is captured in the
    /// shipped content size instead, because for that pane the window size IS
    /// the claim (``windowSize(for:)``).
    private static let windowSize = NSSize(width: 660, height: 581)

    /// The window this pane is captured in.
    ///
    /// # Peers is captured in the window an operator actually has
    ///
    /// It used to be GROWN for this one pane, to the pane's own estimate with
    /// the disclosure open, clamped to the screen, so that every control was
    /// in one frame. That made the capture unfalsifiable about the one thing
    /// the pane is being judged on: in a window taller than the document,
    /// "document ≤ viewport" is true by construction and says nothing about the
    /// 540 pt window `SettingsWindowController` opens. The pane's height budget
    /// makes the pane FIT, so the capture is taken in the shipped window and the
    /// printed line becomes the claim: document against viewport, in the hole
    /// the pane has to fit.
    ///
    /// The Advanced disclosure is closed in this scene (it is `@State` and no
    /// render run opens it), so nothing below the fold is lost by not growing:
    /// what the grown window used to reveal was the pane's own overflow.
    private static func windowSize(for tab: SettingsTab, sheet: SheetScene?) -> NSSize {
        // A SHEET capture is not the fit claim. The mockup measures scene 63's
        // card at 844.61 pt and says the window grows to hold it, which is
        // what a macOS sheet does; capturing it in the 540 pt pane window
        // would clip the bottom of the sheet and the PNG would be a picture of
        // this harness's window rather than of the sheet.
        if sheet != nil { return NSSize(width: 660, height: 980) }
        guard tab == .peers else { return windowSize }
        return SettingsWindowController.shippedContentSize
    }

    /// The window's own title bar and the form's outer margins, everything
    /// around the pane's own rows.
    private static let chromeHeight: CGFloat = 81

    /// The height of the row a pane has to have in frame, at its BOTTOM.
    ///
    /// # What this replaces, and why a constant could not stay
    ///
    /// This used to be `tab == .peers ? 470 : 0`, measured once against the
    /// rendered PNG. The pane it was measured against is gone: the shortened
    /// pane moved Paste a key, Regenerate and the id behind a
    /// disclosure, so 470 pt now scrolls a pane that FITS into the bounce
    /// region and the capture comes back white, while the harness still
    /// prints a successful render. A number aimed at a moving target goes
    /// wrong silently every time the target moves.
    ///
    /// # Why a height and not an accessibility identifier
    ///
    /// Aiming at a NAMED element was tried first and measured, not assumed.
    /// Neither route to one exists in this process (probed 2026-09-18):
    ///
    ///  - `NSView.accessibilityIdentifier()` is empty on every view under the
    ///    pane. SwiftUI's `.accessibilityIdentifier(_:)` does not reach the
    ///    backing views, and a grouped `Form`'s rows are drawn into graphics
    ///    layers, no view in the tree carried the identifier, and none
    ///    carried the row's TEXT either.
    ///  - the accessibility TREE is empty: `accessibilityChildren()` on the
    ///    window's content view answered nil, because AX children are built
    ///    lazily when an AX client attaches and this harness is not one.
    ///
    /// So the harness asks the PANE instead. For Peers the element is the
    /// `Advanced…` disclosure, and the fact that makes it derivable is
    /// structural: **it is the LAST row on the pane**. Its bottom is the
    /// document's bottom, and its height is
    /// ``PeersSettingsPane/lastRowHeight``, off the same metrics
    /// `PeerPaneLayoutTests` gates. Nothing here is measured off a PNG and
    /// nothing is a literal.
    private static func scrollTargetHeight(for tab: SettingsTab) -> CGFloat? {
        tab == .peers ? PeersSettingsPane.lastRowHeight : nil
    }

    /// What the pane's own arithmetic says it needs, for the scene being
    /// captured: the fixture's two trusted Macs and one pairing request, with
    /// the disclosure closed, which is the state `@State private var
    /// advancedOpen = false` puts every render run in.
    ///
    /// Printed beside the drawn document height rather than used to size
    /// anything. Two models of one pane, and either one alone is a claim: the
    /// arithmetic is what the tests can gate, the drawn figure is what the
    /// operator gets.
    private static func paneEstimate(for tab: SettingsTab) -> CGFloat {
        guard tab == .peers else { return 0 }
        return PeersSettingsPane.estimatedHeight(
            trustedMacs: 2, pendingKnocks: 1, advancedOpen: false)
    }

    /// The pane's own scroll view: the WIDEST one in the window, which is the
    /// detail side's `Form`. The sidebar is a `List` and therefore a scroll
    /// view too, and it is the one a plain depth-first walk finds first:
    /// scrolling that instead would move the five row labels and leave the
    /// pane exactly where it was, which looks identical to a scroll that did
    /// not happen.
    private static func paneScrollView(in view: NSView) -> NSScrollView? {
        var found: [NSScrollView] = []
        func walk(_ v: NSView) {
            if let scroll = v as? NSScrollView { found.append(scroll) }
            v.subviews.forEach(walk)
        }
        walk(view)
        return found.max { $0.bounds.width < $1.bounds.width }
    }

    @MainActor
    private static func render(
        _ tab: SettingsTab, appearance: Appearance, sheet: SheetScene? = nil,
        into directory: URL
    ) -> Bool {
        // Two appearances, two mechanisms: `NSApp.appearance` is what the window
        // and its title bar adopt, and the DRAWING appearance is what every
        // dynamic `NSColor` in the view tree resolves against.
        let previousAppAppearance = NSApp.appearance
        NSApp.appearance = appearance.nsAppearance
        // Set and cleared around this ONE capture, so a scene cannot leak into
        // the next one or into a pane that was asked for no sheet at all.
        requestedSheet = sheet
        defer {
            NSApp.appearance = previousAppAppearance
            requestedSheet = nil
        }

        return withDrawingAppearance(appearance.nsAppearance) {
            renderUnderCurrentAppearance(
                tab, appearance: appearance, sheet: sheet, into: directory)
        }
    }

    /// The body of ``render(_:appearance:into:)``, run with the drawing
    /// appearance already installed.
    ///
    /// A separate function only because the macOS 12 replacement for assigning
    /// `NSAppearance.current` takes a block (``withDrawingAppearance(_:perform:)``):
    /// wrapping the body in a closure would have re-indented the whole function
    /// to change nothing.
    @MainActor
    private static func renderUnderCurrentAppearance(
        _ tab: SettingsTab, appearance: Appearance, sheet: SheetScene?, into directory: URL
    ) -> Bool {
        let fleet = Fleet(accounts: fixtureAccounts())
        let dependencies = SettingsDependencies(
            poller: StatusPoller(pinnedState: .loaded(fleet), lastPollAt: Date()),
            server: ServerController(),
            loginItem: LoginItem(),
            awake: AwakeController.harness(),
            preference: LaunchPreference(),
            countsPreference: MenuBarCountsPreference(),
            runningToolsPreference: ShowRunningToolsPreference(),
            defaultTabPreference: DefaultTabPreference(),
            panelDensityPreference: PanelDensityPreference(),
            groupController: GroupController(),
            updater: Updater(startingUpdater: false),
            onWhatsNew: {})

        // A fresh navigation object per capture, not `.shared` — so setting
        // `selectedTab` here can never race or persist against a later call
        // in this same process.
        let navigation = SettingsNavigation()
        navigation.selectedTab = tab

        // The REAL root view — sidebar, toolbar, detail, exactly what
        // `SettingsWindowController` shows — not a bare pane. See this
        // type's own doc-comment for the two rejected approaches that only
        // captured the detail content.
        let rootView = SettingsRootView(dependencies: dependencies, navigation: navigation)
            .environment(\.colorScheme, appearance == .dark ? .dark : .light)

        let hostingController = NSHostingController(rootView: rootView)
        let windowSize = windowSize(for: tab, sheet: sheet)
        let window = NSWindow(
            contentRect: NSRect(origin: .zero, size: windowSize),
            styleMask: [.titled, .closable, .resizable, .miniaturizable, .fullSizeContentView],
            backing: .buffered,
            defer: false)
        window.appearance = appearance.nsAppearance
        window.contentViewController = hostingController
        window.setFrame(NSRect(origin: .zero, size: windowSize), display: true)
        // A normal on-screen position — centred, the way a real window would
        // open — not the screen's literal (0,0) origin. `NSWindow.center()`
        // measured, not assumed: an earlier attempt set the origin to the
        // screen's own `minX`/`minY` (AppKit's bottom-LEFT origin), which put
        // the window's bottom edge at the very bottom of the display, mostly
        // hidden behind the Dock — the captured PNG showed only a drop-shadow
        // gradient in two corners, nothing else. `orderFrontRegardless()`
        // rather than `makeKeyAndOrderFront` and `NSApp.activate` never
        // called — this must never take key focus or activate the app
        // (`NSApp.setActivationPolicy` is never touched anywhere in this
        // file, so it stays `.accessory` throughout).
        window.center()
        window.orderFrontRegardless()
        window.layoutIfNeeded()
        window.contentView?.layoutSubtreeIfNeeded()
        // Several run-loop turns so the window server actually composites a
        // frame before capture.
        for _ in 0..<5 {
            RunLoop.current.run(until: Date().addingTimeInterval(0.05))
        }

        // Then scroll, for the one pane that does not fit in any window this
        // display can hold, and give it its own turns to composite. After the
        // first five: the `Form`'s table view has no document height to scroll
        // within until it has laid out once, so a scroll issued before them
        // clamps to zero and captures the top of the pane.
        if let rowHeight = scrollTargetHeight(for: tab), let contentView = window.contentView,
            let scroll = paneScrollView(in: contentView)
        {
            let documentHeight = scroll.documentView?.frame.height ?? 0
            // Where the clip view RESTS before anything scrolls it. In a
            // `.fullSizeContentView` window that is `-toolbarInset`, not zero,
            // and scrolling to a literal zero is what hid the first section
            // head behind the title bar, ``RenderScrollTarget/clipOrigin(restingY:offset:)``
            // carries the measurement.
            let restingY = scroll.contentView.bounds.origin.y
            // How much of the DOCUMENT an operator sees without touching the
            // wheel: the clip view's own frame, less the inset the toolbar
            // takes off the top.
            //
            // Not `contentView.bounds.height`. That is the frame PLUS the
            // inset (592 for a 540 pt clip with a 52 pt toolbar, measured
            // 2026-09-18), so using it overstated the visible height by 104 pt
            // and made this line's "document ≤ viewport" read as a fit when
            // the pane in fact scrolled by 100. It also under-scrolled every
            // capture by the same amount: the `Advanced…` row this scroll
            // exists to show was still below the fold in the PNG while the log
            // said the pane fit.
            let viewportHeight = scroll.contentView.frame.height + restingY
            // The last row's extent in DOCUMENT coordinates, top-down: its
            // BOTTOM is the document's bottom, by the structural fact
            // ``scrollTargetHeight(for:)`` states.
            let offset = RenderScrollTarget.offset(
                targetMinY: max(0, documentHeight - rowHeight), targetMaxY: documentHeight,
                viewportHeight: viewportHeight, documentHeight: documentHeight)
            scroll.contentView.scroll(
                to: NSPoint(
                    x: 0, y: RenderScrollTarget.clipOrigin(restingY: restingY, offset: offset)))
            scroll.reflectScrolledClipView(scroll.contentView)
            // Printed either way, including the zero: "scrolled to 0" says the
            // pane FIT, which is scene 59's own claim and a fact worth having
            // in the log. A silent skip reads exactly like a scroll that did
            // not happen.
            //
            // The pane's own ESTIMATE is printed beside the drawn figure
            // because they are two different models of one pane and the gap
            // between them is the gap this pane's shrink closed: the arithmetic
            // `PeerPaneLayoutTests` gates against a number only AppKit can
            // produce.
            print(
                "  \(tab.rawValue): scrolled to \(Int(offset)) pt to show the last "
                    + "\(Int(rowHeight)) pt of the pane (document \(Int(documentHeight)) pt, "
                    + "viewport \(Int(viewportHeight)) pt, resting \(Int(restingY)) pt, "
                    + "pane estimate \(Int(paneEstimate(for: tab))) pt)")
            for _ in 0..<3 {
                RunLoop.current.run(until: Date().addingTimeInterval(0.05))
            }
        }

        // A sheet scene is named for the SHEET, not for the pane behind it.
        let name = "\(sheet?.rawValue ?? tab.rawValue)-\(appearance.rawValue).png"
        // `NSView.cacheDisplay`/`ImageRenderer` both draw OFFSCREEN, bypassing
        // the window server entirely — measured across every attempt in this
        // file's own doc-comment, neither one reliably reproduces what
        // `Form(.grouped)`'s native table/vibrancy backing actually composites.
        // `CGWindowListCreateImage` instead asks the WINDOW SERVER for the
        // pixels it already composited for this exact window — the same
        // mechanism `screencapture -l <windowNumber>` uses. Capturing a
        // window this PROCESS OWNS needs no Screen Recording permission
        // (that gate is for capturing another process's windows); only this
        // process's own window is ever named here.
        // A sheet is a WINDOW of its own, attached to this one, so capturing
        // the parent's window number would picture the pane with a grey scrim
        // over it and no sheet at all. `attachedSheet` is nil until AppKit has
        // presented it, which the run-loop turns above are what pay for.
        var target = window
        while let attached = target.attachedSheet { target = attached }
        if sheet != nil && target === window {
            // Said out loud rather than captured anyway: a silent fall back to
            // the parent window writes a PNG that looks like a successful
            // render of the wrong thing.
            FileHandle.standardError.write(
                Data("no sheet presented for \(name); captured nothing\n".utf8))
            window.close()
            return false
        }
        guard
            let cgImage = CGWindowListCreateImage(
                .null, .optionIncludingWindow, CGWindowID(target.windowNumber),
                [.boundsIgnoreFraming, .bestResolution])
        else {
            window.close()
            FileHandle.standardError.write(Data("render failed: \(name)\n".utf8))
            return false
        }
        window.close()
        let rep = NSBitmapImageRep(cgImage: cgImage)
        guard let png = rep.representation(using: .png, properties: [:]) else {
            FileHandle.standardError.write(Data("render failed: \(name)\n".utf8))
            return false
        }

        let url = directory.appendingPathComponent(name)
        do {
            try png.write(to: url)
            print("  \(name)  \(cgImage.width)x\(cgImage.height)px")
            return true
        } catch {
            FileHandle.standardError.write(Data("write failed \(name): \(error)\n".utf8))
            return false
        }
    }
}

/// Hand-built account with the fields this harness's fixtures touch — same
/// shape as `GroupSummaryTests`'s own `summaryAccount` fixture.
private func fixtureAccount(
    _ name: String,
    groups: [String]?,
    reservedGroups: [String]? = nil,
    parkedGroups: [String]? = nil,
    groupColors: [String: String]? = nil
) -> Account {
    Account(
        name: name,
        priority: 1,
        status: "active",
        disabled: false,
        quota: 0.2,
        quotaState: .ok,
        fiveHour: 0.2,
        sevenDay: 0.2,
        sevenDayOi: nil,
        held: [],
        requests: 10,
        inputTokens: 1000,
        outputTokens: 500,
        cacheReadTokens: 200,
        cacheHitRatio: 0.9,
        probeStatus: .ok,
        probeError: nil,
        lastStreamError: nil,
        streamErrorCount: 0,
        source: .live,
        serverSha: "abc1234",
        serverDirty: false,
        groups: groups,
        reservedGroups: reservedGroups,
        parkedGroups: parkedGroups,
        groupColors: groupColors
    )
}
