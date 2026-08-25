import AppKit
import Combine
import TcrBarCore

/// `TcrBar --shell-probe` — build the real menu-bar shell in-process, exercise
/// it, print one line per assertion, and exit non-zero if any of them failed.
///
/// ## Why a flag and not a test
///
/// The test target links `TcrBarCore` only, and none of what this checks is a
/// fact about types. It is what AppKit actually draws once a real
/// `NSStatusBarButton` has laid itself out and a real `NSPopover` has sized
/// itself — the same class of thing that `--render-states` exists for, and the
/// same class of thing that shipped every genuine bug this app has had.
///
/// The obvious alternative, looking at the menu bar, is not available:
/// `screencapture` needs Screen Recording, which is not granted on the machine
/// this was written on, and a build machine or a headless agent will not have it
/// either.
///
/// ## What it does NOT cover — read this before trusting a green run
///
///  - It rasterises **the button**, via `bitmapImageRepForCachingDisplay` +
///    `cacheDisplay`, not the window server's final composite of the menu bar.
///    Anything the compositor does afterwards — a vibrancy pass, the highlight
///    fill under an open panel, the menu bar's own reduced-transparency
///    treatment — happens past this measurement.
///  - It clicks nothing with a real mouse. It calls `openPanel()` — the same
///    method the button's action calls — which is not the same as proving the
///    target/action wiring survives a real click.
///  - It says nothing about a *bundled* run: no `LSUIElement`, no code signature,
///    no login item. `scripts/build-tcrbar.sh` and a human's eyes still own that.
///  - It does not exercise the **animated** dismissal. The probe sets
///    `popover.animates = false` before it closes anything, for the reason given
///    at that line, so the code path a human sees — the panel fading out — is
///    reported and not asserted here.
///
/// What still needs human eyes, in the menu bar of a machine that is awake and
/// unlocked: the icon is actually there and is the right colour in both states,
/// a real click opens the panel, and **open the panel and watch it animate
/// away** — that last one is the part this file deliberately stops measuring.
///
/// It is not invisible while it runs: a status item appears in the real menu bar
/// for a couple of seconds, and `openPanel()` activates the app and shows a real
/// popover. It touches no server and holds no power assertion.
///
/// ## Every assertion here can fail, and that was checked
///
/// A check that cannot fail is worse than no check, and this project has shipped
/// that exact class — which is why `KeepAwakeProbe` has a deliberate
/// post-release linger. Each of the nine below was broken on purpose, watched go
/// red, and restored. Assertion 4 is the negative control for assertion 3:
/// without it, "the ON mark has cyan in it" passes just as happily on a mark
/// that is cyan in both states, which would mean the two states look identical
/// in the menu bar.
enum ShellProbe {
    static let flag = "--shell-probe"

    static func requested(_ arguments: [String] = CommandLine.arguments) -> Bool {
        arguments.contains(flag)
    }

    /// Nothing here should take a second. If the run loop wedges — a popover that
    /// never opens, a status bar that never vends a button — the probe has to
    /// fail rather than hang a CI step forever.
    ///
    /// It has to clear the sum of everything below it, or a single failing
    /// assertion is replaced by "nothing concluded", which is a strictly worse
    /// diagnostic. Four `waitUntil` calls at 5s each is 20s of worst case, plus
    /// about 2.5s of `settle`, so 30s left barely 7s of headroom and one more
    /// waiting assertion would have eaten it.
    private static let deadline: TimeInterval = 60

    @MainActor
    static func run() -> Never {
        let app = NSApplication.shared
        app.setActivationPolicy(.accessory)

        DispatchQueue.main.asyncAfter(deadline: .now() + deadline) {
            FileHandle.standardError.write(
                Data("shell-probe: deadline after \(Int(deadline))s — nothing concluded\n".utf8))
            exit(2)
        }

        Task { @MainActor in
            await exercise()  // exits
        }
        app.run()
        exit(2)
    }

    // MARK: - The run

    @MainActor
    private static func exercise() async -> Never {
        // A pinned poller: no timer, no `tcr` subprocess, and a fleet big enough
        // that a collapsed scroll view is numerically obvious. An inert
        // keep-awake: rasterising a cup must not stop this machine sleeping. An
        // unstarted updater for the same reason both of those are substituted: a
        // started one schedules background checks and can put an update window
        // on screen, and a probe that is measuring popover geometry must not have
        // Sparkle opening its own window underneath the measurement.
        let shell = MenuBarShell(
            poller: StatusPoller(pinnedState: .loaded(probeFleet())),
            awake: AwakeController(activity: .inert),
            updater: Updater(startingUpdater: false))

        var checks: [Check] = []
        var notes: [String] = []
        var occlusionAtOpen = "not reached"
        await settle()

        // 1 — the status item exists, is visible, and its button does something
        //     when clicked.
        //
        //     **`isVisible` is asserted, and an earlier version of this comment
        //     was wrong to excuse it.** It claimed the flag "reads false for this
        //     probe in a normal, working run" and that asserting it "fails a
        //     correct shell". It does not. `isVisible` is persisted per status
        //     item in the app's defaults domain, and it read false here because
        //     the domain held a stale `"NSStatusItem VisibleCC Item-0" = 0` —
        //     which is the shipped bug, not probe noise. Control: five status
        //     items created in the same locked session (bare, title-only,
        //     template-image, image-set-after-a-run-loop-turn, and one built
        //     exactly as `MenuBarShell` builds its own at 32x29) all read
        //     `isVisible=true`, because they were in a domain without the key.
        //     The de-assertion papered over a true positive, and a status item
        //     that draws 754 opaque pixels into a bitmap while being hidden from
        //     the menu bar is precisely the failure a human reports as "the app
        //     did not launch". `MenuBarShell` now sets it at creation.
        //
        //     **bounds** stays reported and not asserted, because it genuinely
        //     cannot fail: `NSStatusBar.system.statusItem(...)` vends a non-nil
        //     `NSStatusBarButton` with a live frame even at `withLength: 0`,
        //     with `isVisible` false, and with no image or title ever set —
        //     measured 32x29, 32x29 and 16x22 respectively.
        //
        //     The rest is what the shell genuinely owns: a status bar button of
        //     the right class whose target/action pair points back here. An
        //     unwired button is an item that ignores every click, which is the
        //     one failure at this layer that a later assertion would not catch.
        let button = shell.statusItem.button
        let buttonClass = button.map { String(describing: type(of: $0)) } ?? "nil"
        let bounds = button?.bounds ?? .zero
        let wired = button.map { $0.target as AnyObject? === shell && $0.action != nil } ?? false
        let visible = shell.statusItem.isVisible
        checks.append(
            Check(
                1, "status item is visible and vends an NSStatusBarButton wired to the shell",
                passed: button != nil && buttonClass.contains("StatusBarButton") && wired
                    && visible,
                detail: "class=\(buttonClass) wired=\(wired) isVisible=\(visible) "
                    + "(reported, not asserted: "
                    + "bounds=\(Int(bounds.width))x\(Int(bounds.height)))"))

        guard let button else {
            report(checks, environment: environment(shell, occlusionAtOpen: occlusionAtOpen),
                notes: notes)
            exit(1)
        }

        // 2 — OFF is a template image, so the menu bar keeps tinting it.
        let offImage = button.image
        checks.append(
            Check(
                2, "OFF: button.image is set and isTemplate == true",
                passed: offImage != nil && offImage?.isTemplate == true,
                detail: "image=\(offImage == nil ? "nil" : "set") isTemplate="
                    + "\(offImage.map { String($0.isTemplate) } ?? "n/a")"))

        // 4 — the negative control, run before its positive so a leftover ON
        //     state cannot be what makes it pass.
        let offScan = scan(button)
        checks.append(
            Check(
                4, "OFF: rasterised button has 0 cyan pixels (negative control)",
                passed: offScan != nil && offScan?.cyan == 0,
                detail: offScan.map { "opaque=\($0.opaque) cyan=\($0.cyan)" } ?? "no bitmap"))

        // 3 and 8 — the whole point of owning the button.
        shell.awake.setOn(true)
        await settle()
        let onScan = scan(button)
        checks.append(
            Check(
                3, "ON: rasterised button has > 0 cyan pixels",
                passed: (onScan?.cyan ?? 0) > 0,
                detail: onScan.map { "opaque=\($0.opaque) cyan=\($0.cyan)" } ?? "no bitmap"))
        checks.append(
            Check(
                8, "ON: the gauge survived — > 0 opaque non-cyan pixels",
                passed: (onScan.map { $0.opaque - $0.cyan } ?? 0) > 0,
                detail: onScan.map { "nonCyan=\($0.opaque - $0.cyan)" } ?? "no bitmap"))
        shell.awake.setOn(false)
        await settle()

        // 5 — go through the real `openPanel()` rather than `popover.show`, so
        //     the things that used to ride on the panel being rebuilt per open
        //     are exercised rather than assumed.
        shell.openPanel()
        let opened = await waitUntil {
            shell.popover.isShown && shell.popover.contentSize.height > 300
        }
        occlusionAtOpen = occlusion(of: shell)
        let size = shell.popover.contentSize
        checks.append(
            Check(
                5, "open: isShown, contentSize is \(Int(Tok.panelWidth))pt wide and > 300pt tall",
                passed: shell.popover.isShown && abs(size.width - Tok.panelWidth) < 1
                    && size.height > 300,
                detail: "isShown=\(shell.popover.isShown) timedOut=\(!opened) "
                    + "contentSize=\(Int(size.width))x\(Int(size.height))"))

        // 6 — and it closes again.
        //
        //     `animates = false` first, and this is a substitution in the probe
        //     rather than a change to the app, the same way the pinned poller
        //     and the inert `AwakeController` above are. `popover` is a `let`,
        //     so nothing in `MenuBarShell` moves. **Do not "fix" this assertion
        //     by changing `closePanel()`** — `performClose(nil)` is correct.
        //
        //     Measured, 8 variants under a real `NSApplication.run()`: the sole
        //     discriminator is `animates`. Every `animates = true` variant was
        //     still `isShown == true` six seconds later — `performClose` *and*
        //     `close()`, across `.transient`, `.semitransient` and
        //     `.applicationDefined`, with and without `NSApp.activate()`. Both
        //     `animates = false` variants closed in under 0.1s. A mechanism
        //     probe in the same process narrowed it: an empty
        //     `NSAnimationContext` group completes in 0.1s and a raw
        //     `CATransaction` on the popover's layer in 0.3s, but an alpha
        //     animation on the popover's `_NSPopoverWindow` — and on an ordinary
        //     `NSWindow` — never completes in 3s. `NSPopover` flips `isShown`
        //     inside that window-animation completion, so with nothing being
        //     composited (screen locked, display asleep, `occlusionState`
        //     without `.visible`) it never flips.
        //
        //     So the animated variant of this assertion is not a check that
        //     cannot fail; it is the more dangerous inverse — one that goes red
        //     on a correct app whenever the machine is not drawing, and whose
        //     obvious fix is a regression. The environment line at the bottom of
        //     the report is what makes that adjudicable next time.
        shell.popover.animates = false
        shell.closePanel()
        let closed = await waitUntil { !shell.popover.isShown }
        checks.append(
            Check(
                6, "close: isShown == false",
                passed: !shell.popover.isShown,
                detail: "isShown=\(shell.popover.isShown) timedOut=\(!closed)"))
        notes.append(
            "the animated dismissal was NOT exercised — `popover.animates` was set false before "
                + "assertion 6, because the window animation it waits on does not complete when "
                + "nothing is being composited. Open the panel and watch it animate away.")

        // 9 — the login-item re-read, measured on the SECOND open, which is the
        //     only place it means anything.
        //
        //     `@Published` fires on every assignment, equal or not, so emissions
        //     of `loginItem.$status` are a faithful count of `refresh()` calls.
        //     Counting them on the FIRST open proves nothing, and that was
        //     measured rather than reasoned about: `FleetView` carries its own
        //     `.onAppear { loginItem.refresh() }` (`FleetView.swift:388`), which
        //     fires during the first show and kept this assertion green with the
        //     shell's refresh deleted. It fires once, because one popover keeps
        //     one hosting controller — which is precisely the regression this
        //     assertion exists for, so the second open is where to look.
        //
        //     It is gated on assertion 6's close having actually happened, and
        //     that gate is not decoration. `show(relativeTo:)` on a popover that
        //     is already shown re-anchors it; it does not open it. So whenever
        //     the close silently failed, the "SECOND open" this assertion is
        //     named for never occurred — and it passed anyway, because
        //     `openPanel()` calls `loginItem.refresh()` unconditionally and
        //     `@Published` fires on every assignment. A green line that is green
        //     for a different reason than its own comment gives is worse than a
        //     red one, because the next reader believes the comment.
        let loginReads = Counter()
        let watch = shell.loginItem.$status.dropFirst().sink { _ in loginReads.value += 1 }
        shell.openPanel()
        let reopened = await waitUntil { shell.popover.isShown }
        await settle(0.5)
        let reopenReads = loginReads.value
        watch.cancel()
        checks.append(
            Check(
                9, "re-open: the login-item bit is read again (macOS owns it; a cache is a lie)",
                passed: closed && reopenReads > 0,
                detail: "LoginItem.status emissions on the second open=\(reopenReads) "
                    + "precedingCloseSucceeded=\(closed) timedOut=\(!reopened)"))
        shell.closePanel()
        let closedAgain = await waitUntil { !shell.popover.isShown }
        notes.append("teardown close: isShown=\(shell.popover.isShown) timedOut=\(!closedAgain)")

        // 7 — the claim the ON image rests on: `NSColor.labelColor` inside the
        //     drawing handler resolves in the appearance current at DRAW time,
        //     so the gauge is right in both while the cup stays cyan in both.
        checks.append(appearanceCheck())

        report(checks, environment: environment(shell, occlusionAtOpen: occlusionAtOpen),
            notes: notes)
        exit(checks.allSatisfy(\.passed) ? 0 : 1)
    }

    /// Let the run loop lay out what was just changed. A status item is
    /// `.variableLength`, so its button resizes only after a layout pass, and the
    /// popover's preferred content size arrives from SwiftUI a pass after the
    /// view first appears.
    private static func settle(_ seconds: Double = 0.4) async {
        try? await Task.sleep(nanoseconds: UInt64(seconds * 1_000_000_000))
    }

    /// Poll a condition, returning as soon as it holds or when `timeout` passes.
    ///
    /// Not a "sleep until it goes green": a condition that is genuinely false
    /// stays false and the assertion built on it still fails. It exists because
    /// `NSPopover` animates, and the two waits it guards were measured to be
    /// borderline against a fixed sleep — `performClose` left `isShown` true for
    /// somewhere between 0.5s and 0.75s on this machine, so a 0.4s settle failed
    /// assertion 6 on a popover that had in fact closed.
    private static func waitUntil(
        _ timeout: Double = 5, _ condition: () -> Bool
    ) async -> Bool {
        let step = 0.1
        var waited = 0.0
        while waited < timeout {
            if condition() { return true }
            await settle(step)
            waited += step
        }
        return condition()
    }

    // MARK: - Assertion 7

    @MainActor
    private static func appearanceCheck() -> Check {
        guard
            let mark = MenuBarMark.image(
                gaugeSymbol: "gauge.with.dots.needle.33percent", awake: true,
                awakeTint: Tok.awakeNSColor),
            let aqua = NSAppearance(named: .aqua),
            let dark = NSAppearance(named: .darkAqua),
            let light = rasterise(mark, in: aqua),
            let night = rasterise(mark, in: dark)
        else {
            return Check(
                7, "ON image re-resolves labelColor per appearance", passed: false,
                detail: "could not compose or rasterise the mark")
        }
        // The gauge is whatever is NOT cyan; the cup is whatever is. Splitting
        // by colour rather than by pixel column keeps this from silently
        // measuring the wrong half if the composition ever changes its layout.
        let gaugeMoved = abs(light.nonCyanLuma - night.nonCyanLuma) > 0.2
        let cupHeld = light.cyan > 0 && night.cyan > 0
        return Check(
            7, "ON image: gauge differs between .aqua and .darkAqua, cup cyan in both",
            passed: gaugeMoved && cupHeld,
            detail: String(
                format: "aqua gaugeLuma=%.3f cyan=%d · dark gaugeLuma=%.3f cyan=%d",
                light.nonCyanLuma, light.cyan, night.nonCyanLuma, night.cyan))
    }

    // MARK: - Pixels

    private struct PixelScan {
        var opaque = 0
        var cyan = 0
        /// Mean relative luminance of the opaque pixels that are not cyan — i.e.
        /// of the gauge.
        var nonCyanLuma = 0.0
    }

    /// The technique that produced the 533/533 measurement in ``MenuBarMark``'s
    /// table: ask the button for a bitmap rep of itself and have it draw into it.
    @MainActor
    private static func scan(_ button: NSStatusBarButton) -> PixelScan? {
        let bounds = button.bounds
        guard bounds.width > 0, bounds.height > 0,
            let rep = button.bitmapImageRepForCachingDisplay(in: bounds)
        else { return nil }
        button.cacheDisplay(in: bounds, to: rep)
        return count(rep)
    }

    @MainActor
    private static func rasterise(_ image: NSImage, in appearance: NSAppearance) -> PixelScan? {
        let scale = 2
        let width = Int(image.size.width.rounded()) * scale
        let height = Int(image.size.height.rounded()) * scale
        guard width > 0, height > 0,
            let rep = NSBitmapImageRep(
                bitmapDataPlanes: nil, pixelsWide: width, pixelsHigh: height,
                bitsPerSample: 8, samplesPerPixel: 4, hasAlpha: true, isPlanar: false,
                colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0)
        else { return nil }
        appearance.performAsCurrentDrawingAppearance {
            NSGraphicsContext.saveGraphicsState()
            let context = NSGraphicsContext(bitmapImageRep: rep)
            NSGraphicsContext.current = context
            context?.cgContext.scaleBy(x: CGFloat(scale), y: CGFloat(scale))
            image.draw(
                in: NSRect(origin: .zero, size: image.size), from: .zero,
                operation: .sourceOver, fraction: 1)
            NSGraphicsContext.restoreGraphicsState()
        }
        return count(rep)
    }

    /// `g - r > 0.15 && b - r > 0.15` is the same cyan predicate the six-variant
    /// `MenuBarExtra` sweep used, kept verbatim so the numbers this prints are
    /// comparable with the ones in ``MenuBarMark``'s table.
    private static func count(_ rep: NSBitmapImageRep) -> PixelScan {
        var out = PixelScan()
        var lumaSum = 0.0
        var lumaCount = 0
        for x in 0..<rep.pixelsWide {
            for y in 0..<rep.pixelsHigh {
                guard let colour = rep.colorAt(x: x, y: y)?.usingColorSpace(.sRGB),
                    colour.alphaComponent > 0.35
                else { continue }
                out.opaque += 1
                let r = colour.redComponent, g = colour.greenComponent, b = colour.blueComponent
                if g - r > 0.15 && b - r > 0.15 {
                    out.cyan += 1
                } else {
                    lumaSum += 0.2126 * r + 0.7152 * g + 0.0722 * b
                    lumaCount += 1
                }
            }
        }
        out.nonCyanLuma = lumaCount == 0 ? 0 : lumaSum / Double(lumaCount)
        return out
    }

    // MARK: - Reporting

    /// A box, so a Combine sink can add to something the enclosing scope can
    /// still read afterwards.
    @MainActor
    private final class Counter {
        var value = 0
    }

    private struct Check {
        let number: Int
        let name: String
        let passed: Bool
        let detail: String

        init(_ number: Int, _ name: String, passed: Bool, detail: String) {
            self.number = number
            self.name = name
            self.passed = passed
            self.detail = detail
        }
    }

    /// The state of the machine the run happened on, printed every run whether
    /// or not anything failed.
    ///
    /// This file is otherwise scrupulous about what it measures, and the
    /// environment was the one input it took on faith. That cost most of a day:
    /// two runs of the same binary disagreed about assertion 6 and there was
    /// nothing in either output to adjudicate between them. The three values
    /// here are the ones that actually decide whether AppKit will run a window
    /// animation to completion.
    @MainActor
    private static func environment(_ shell: MenuBarShell, occlusionAtOpen: String) -> String {
        let session = CGSessionCopyCurrentDictionary() as? [String: Any]
        let locked: String
        switch session?["CGSSessionScreenIsLocked"] {
        case let flag as Bool: locked = String(flag)
        case let flag as Int: locked = String(flag != 0)
        default: locked = "absent"
        }
        return "screenLocked=\(locked) appActive=\(NSApp.isActive) "
            + "popoverWindowOcclusion(atOpen)=\(occlusionAtOpen) "
            + "popoverWindowOcclusion(now)=\(occlusion(of: shell))"
    }

    /// `.visible` on the popover's own window. Sampled while the panel is open,
    /// because the window goes away with it.
    @MainActor
    private static func occlusion(of shell: MenuBarShell) -> String {
        guard let window = shell.popover.contentViewController?.view.window else {
            return "no-window"
        }
        return window.occlusionState.contains(.visible) ? "visible" : "not-visible"
    }

    /// One line per assertion, and the verdict grep-able on its own line.
    private static func report(_ checks: [Check], environment: String, notes: [String]) {
        for check in checks.sorted(by: { $0.number < $1.number }) {
            print("shell-probe: \(check.passed ? "PASS" : "FAIL") \(check.number). "
                + "\(check.name) — \(check.detail)")
        }
        for note in notes {
            print("shell-probe: NOTE \(note)")
        }
        print("shell-probe: env \(environment)")
        let failed = checks.filter { !$0.passed }
        print(
            "shell-probe: \(checks.count - failed.count)/\(checks.count) passed"
                + (failed.isEmpty ? "" : " — failed \(failed.map(\.number).sorted())"))
        fflush(stdout)
    }

    // MARK: - Fixture
    //
    // Fake accounts only. This repository is public and real account addresses
    // never enter it.

    /// Thirteen rows, which is the shape that made the scroll view collapse. A
    /// two-account fleet would let assertion 5 pass on a panel that is short for
    /// an honest reason.
    private static func probeFleet() -> Fleet {
        // Relative, not a fixed epoch: `QuotaFormat.resetCaption` refuses a
        // reset that is not in the future, so a hardcoded timestamp would draw
        // the Fable caption today and drop it silently a month from now —
        // shortening the very line whose width this probe exists to check.
        let fableReset = Int64(Date().addingTimeInterval(4.5 * 86_400).timeIntervalSince1970 * 1000)
        let rows = (1...13).map { index in
            """
            {"name":"probe-\(index)@example.com","priority":0,"status":"active",
             "disabled":false,"quota":0.\(index % 9 + 1),"quotaState":"ok",
             "fiveHour":0.1,"sevenDay":0.1,
             "sevenDayOi":0.1,"sevenDayOiState":"ok","sevenDayOiResetAtMs":\(fableReset),
             "held":[],
             "requests":1,"inputTokens":1,"outputTokens":1,"cacheReadTokens":1,
             "cacheCreationTokens":0,"cacheHitRatio":0.5,"probeStatus":"ok",
             "probeError":null,
             "lastStreamError":null,"streamErrorCount":0,"source":"live",
             "serverSha":"abc1234","serverDirty":false,
             "usage":{"today":{"requests":1,"inputTokens":1,"cacheCreationTokens":0,
               "cacheCreation1hTokens":0,"cacheReadTokens":1,"outputTokens":1,
               "costUsd":0.42,"unpricedRequests":0},
              "window":{"requests":1,"inputTokens":1,"cacheCreationTokens":0,
               "cacheCreation1hTokens":0,"cacheReadTokens":1,"outputTokens":1,
               "costUsd":0.42,"unpricedRequests":0,"since":1767207600000},
              "lastHour":{"requests":1,"inputTokens":1,"cacheCreationTokens":0,
               "cacheCreation1hTokens":0,"cacheReadTokens":1,"outputTokens":1,
               "costUsd":0.42,"unpricedRequests":0},
              "todayByModel":{"claude-opus-5":{"requests":1,"inputTokens":1,
                "cacheCreationTokens":0,"cacheCreation1hTokens":0,"cacheReadTokens":1,
                "outputTokens":1,"costUsd":0.42,"unpricedRequests":0}}}}
            """
        }
        let json = "[\(rows.joined(separator: ","))]"
        return (try? Fleet.decode(Data(json.utf8))) ?? Fleet(accounts: [])
    }
}
