import AppKit
import Combine
import TcrBarCore

/// `TcrBar --measure-open` prints what one panel open costs, step by step, on
/// the machine it is run on.
///
/// ## Why a flag and not a test
///
/// The same reason `--shell-probe` is a flag. None of this is a fact about
/// types: it is what a real `tcr` on this machine answers in, what the file
/// and kernel reads cost against this machine's own session directory and
/// process table, and what SwiftUI charges to build the panel the first time a
/// real `NSPopover` asks for it. A test with a stub would report the stub.
///
/// ## What it runs
///
/// Read verbs only: `status --json`, `sessions --json`, `peer ls --json`,
/// `peer status --json`, `control --show`. Nothing here changes a file, a
/// group, an account or a peer, and nothing here starts or signals a server.
/// The proxy it reads may be serving live traffic; five read verbs are what
/// the panel itself already runs every three seconds.
///
/// ## What it does NOT cover
///
///  - It does not click the menu bar item. It calls `openPanel()`, the same
///    method the button's action calls, the same substitution `--shell-probe`
///    makes and documents.
///  - The numbers are one machine's, with whatever else that machine is doing
///    at the time. Three rounds are printed rather than one average, so a
///    round that caught a busy moment is visible as itself.
///  - It says nothing about a bundled run.
///
/// It is not invisible while it runs: the last part builds the real shell, so
/// a status item appears in the menu bar for a few seconds and a real panel
/// opens and closes.
enum MeasureOpen {
    static let flag = "--measure-open"

    static func requested(_ arguments: [String] = CommandLine.arguments) -> Bool {
        arguments.contains(flag)
    }

    /// Three, so one slow round is visible as one slow round rather than
    /// being averaged into a number that describes no run that happened.
    private static let rounds = 3

    /// Enough for the slowest thing here (a first panel build) with room to
    /// spare. A harness that hangs reports nothing, which is worse than a
    /// harness that reports a timeout.
    private static let deadline: TimeInterval = 120

    @MainActor
    static func run() -> Never {
        let app = NSApplication.shared
        app.setActivationPolicy(.accessory)

        DispatchQueue.main.asyncAfter(deadline: .now() + deadline) {
            FileHandle.standardError.write(
                Data("measure-open: deadline after \(Int(deadline))s, nothing concluded\n".utf8))
            exit(2)
        }

        Task { @MainActor in
            await measure()  // exits
        }
        app.run()
        exit(2)
    }

    // MARK: - The run

    @MainActor
    private static func measure() async -> Never {
        guard case .success(let executable) = TcrTool.resolve() else {
            print("measure-open: tcr not found, nothing to measure")
            fflush(stdout)
            exit(1)
        }

        print("measure-open: resolved tcr at \(executable.path)")
        for round in 1...rounds {
            await measureReads(round: round, executable: executable)
        }
        await measureOpen()

        print("measure-open: done")
        fflush(stdout)
        exit(0)
    }

    /// Every read one panel open performs, first on its own and then in the
    /// groups the app actually runs them in.
    ///
    /// The single `tcr` reads are timed OFF the main thread, in one detached
    /// task, because that is where the app runs every one of them and because
    /// the main thread charges for them differently: `Process.waitUntilExit()`
    /// spins the run loop, and the same child measured on the main thread here
    /// came back about ten times its own cost. The main-thread control below
    /// keeps that visible instead of hiding it in an average.
    @MainActor
    private static func measureReads(round: Int, executable: URL) async {
        for measured in await Task.detached(priority: .userInitiated, operation: {
            offMainReads(executable: executable)
        }).value {
            line(round, measured.step, measured.took, measured.note)
        }
        line(
            round, "run status --json on the main thread",
            seconds(of: { _ = try? TcrTool.run(executable: executable, arguments: statusRead) }),
            "control: the run loop is what the wait costs")
        line(round, "read session files", seconds(of: { _ = SessionFiles.read() }), "main thread")
        line(round, "read machine stats", seconds(of: { _ = MachineStats.read() }), "main thread")
        line(round, "read process table", seconds(of: { _ = ProcessTable.read() }), "main thread")
        line(
            round, "read claude route", seconds(of: { _ = ClaudeRouteRead.current() }),
            "main thread")

        let loginItem = LoginItem()
        line(round, "read login item", seconds(of: { loginItem.refresh() }), "main thread")

        let poller = StatusPoller()
        line(
            round, "one whole poll, status and sessions",
            await seconds(of: { _ = await poller.pollOnce() }), "off main thread")

        let peers = PeerController()
        line(
            round, "one whole peers read, peer ls and peer status",
            await seconds(of: { await peers.refresh() }), "off main thread")

        line(
            round, "group panel reads, five in a row",
            seconds(of: {
                _ = SessionFiles.read()
                _ = MachineStats.read()
                _ = ProcessTable.read()
                _ = ProcessTable.read()
                _ = ClaudeRouteRead.current()
            }), "main thread, every poll tick")
    }

    /// The thing the owner actually sees: a real shell, a live poller, and the
    /// same `openPanel()` the status item's action calls.
    ///
    /// Two opens, because they are not the same event. `NSHostingController`
    /// builds its view lazily, so the FIRST open is where SwiftUI constructs
    /// the whole panel and where `FleetView`'s `onAppear` runs; the second
    /// re-shows a controller that already exists.
    @MainActor
    private static func measureOpen() async {
        // A live poller: this part is measuring a real open, and an open over
        // a pinned fleet would measure a panel nobody has. Everything else is
        // substituted exactly as `--shell-probe` substitutes it and for the
        // same reasons: an inert keep-awake must not stop this machine
        // sleeping, an unstarted updater must not put a window under the
        // measurement, and a fetcher that reaches nothing keeps the run off
        // the network. The server controller is built and never started.
        let shell = MenuBarShell(
            awake: AwakeController.harness(),
            updater: Updater(startingUpdater: false),
            whatsNew: WhatsNewController(
                store: WhatsNewStore(defaults: nil), fetcher: NeverFetches(),
                location: nil, currentVersion: nil))
        shell.poller.start()
        let waited = await seconds(of: {
            _ = await waitUntil { shell.poller.state.isHealthyRead }
        })
        line(0, "poller first healthy read", waited, "before any open")

        let firstCall = seconds(of: { shell.openPanel() })
        let firstSized = await seconds(of: {
            _ = await waitUntil { shell.popover.isShown && shell.popover.contentSize.height > 300 }
        })
        line(0, "open 1, the openPanel call", firstCall, "main thread, blocking")
        line(0, "open 1, wait until drawn at size", firstSized, "after the call returned")
        print(
            "measure-open: open 1 contentSize="
                + "\(Int(shell.popover.contentSize.width))x"
                + "\(Int(shell.popover.contentSize.height)) isShown=\(shell.popover.isShown)")

        // `animates = false` before closing, the substitution `--shell-probe`
        // documents at length: an animated dismissal never completes when
        // nothing is being composited, and this harness would then measure a
        // second open that never happened.
        shell.popover.animates = false
        shell.closePanel()
        _ = await waitUntil { !shell.popover.isShown }

        let secondCall = seconds(of: { shell.openPanel() })
        let secondSized = await seconds(of: {
            _ = await waitUntil { shell.popover.isShown && shell.popover.contentSize.height > 300 }
        })
        line(0, "open 2, the openPanel call", secondCall, "main thread, blocking")
        line(0, "open 2, wait until drawn at size", secondSized, "after the call returned")

        // The step that answers the complaint, and the only one here whose
        // number a person could have told you without a stopwatch: how long
        // after the click the figures on screen stop being the last tick's.
        //
        // Timed from the middle of the poll interval rather than from wherever
        // the timer happened to be, so the run is repeatable and the number is
        // the average case rather than a lucky or unlucky one: wait for a tick
        // to land, sleep half an interval, then open. Whatever advances
        // `lastPollAt` first is what the reader waited for.
        shell.popover.animates = false
        shell.closePanel()
        _ = await waitUntil { !shell.popover.isShown }
        let lastTick = shell.poller.lastPollAt
        _ = await waitUntil { shell.poller.lastPollAt != lastTick }
        try? await Task.sleep(
            nanoseconds: UInt64(StatusPoller.defaultInterval / 2 * 1_000_000_000))
        let anchor = shell.poller.lastPollAt
        let fresh = await seconds(of: {
            shell.openPanel()
            _ = await waitUntil { shell.poller.lastPollAt != anchor }
        })
        line(0, "open 3, until the fleet on screen is fresh", fresh, "opened mid interval")
        // Last, and only after both opens: the two blocking calls `openPanel`
        // makes before it shows anything. Timed here rather than before open 1
        // because activating an app that is already frontmost is not the same
        // call, and measuring it first would have made open 1 cheaper than the
        // open it is standing in for.
        line(
            0, "activate the app", seconds(of: { NSApp.activate(ignoringOtherApps: true) }),
            "inside openPanel, already frontmost")
        line(
            0, "read login item", seconds(of: { shell.loginItem.refresh() }),
            "inside openPanel, before the draw")
        shell.closePanel()
        shell.poller.stop()
        shell.statusItem.isVisible = false
    }

    // MARK: - Plumbing

    /// One timed step, carried back from the thread that timed it.
    private struct Measured: Sendable {
        let step: String
        let took: Double
        let note: String
    }

    /// The reads that spawn a child, timed where the app runs them.
    private nonisolated static func offMainReads(executable: URL) -> [Measured] {
        var out: [Measured] = []
        out.append(
            Measured(
                step: "resolve tcr", took: seconds(of: { _ = TcrTool.resolve() }),
                note: "once per read"))
        // The control for every `tcr` line below it: a child that does nothing
        // at all, run through the same `TcrTool.run`, so what a spawn costs
        // this process is separated from what the CLI costs. Without it a slow
        // read and a slow spawn are the same number.
        out.append(
            Measured(
                step: "spawn floor, a child that exits at once",
                took: seconds(of: { _ = try? TcrTool.run(executable: noOpChild, arguments: []) }),
                note: "every read pays this"))
        let reads: [(String, [String], String)] = [
            ("run status --json", statusRead, "poll group"),
            ("run sessions --json", sessionsRead, "poll group"),
            ("run peer ls --json", PeerCommand.list, "peers group"),
            ("run peer status --json", PeerCommand.liveStatus, "peers group"),
            ("run control --show", ControlAccountCommand.showArguments, "open path"),
        ]
        for (step, arguments, note) in reads {
            out.append(
                Measured(
                    step: step,
                    took: seconds(of: {
                        _ = try? TcrTool.run(executable: executable, arguments: arguments)
                    }),
                    note: note))
        }
        return out
    }

    private static let statusRead = ["status", "--json"]
    private static let sessionsRead = ["sessions", "--json"]

    /// A child that starts and exits, for the spawn-cost control.
    private static let noOpChild = URL(fileURLWithPath: "/usr/bin/true")

    /// Wall clock, in seconds, around a blocking call.
    private static func seconds(of body: () -> Void) -> Double {
        let started = DispatchTime.now().uptimeNanoseconds
        body()
        return Double(DispatchTime.now().uptimeNanoseconds - started) / 1_000_000_000
    }

    private static func seconds(of body: () async -> Void) async -> Double {
        let started = DispatchTime.now().uptimeNanoseconds
        await body()
        return Double(DispatchTime.now().uptimeNanoseconds - started) / 1_000_000_000
    }

    /// Poll a condition until it holds. Not a sleep until green: a condition
    /// that is false stays false and the number printed is the timeout, which
    /// is why the step names say what they waited for.
    private static func waitUntil(_ timeout: Double = 20, _ condition: () -> Bool) async -> Bool {
        let step = 0.02
        var waited = 0.0
        while waited < timeout {
            if condition() { return true }
            try? await Task.sleep(nanoseconds: UInt64(step * 1_000_000_000))
            waited += step
        }
        return condition()
    }

    /// One line per step, in the shape the rest of this app's harnesses print:
    /// a prefix to grep for, the step, the number, and what the number is a
    /// cost of. `round` 0 is the part that runs once.
    private static func line(_ round: Int, _ step: String, _ took: Double, _ note: String) {
        let phase = round == 0 ? "once" : "round \(round)"
        print(String(format: "measure-open: %@ · %@ · ms=%.1f · %@", phase, step, took * 1000, note))
        fflush(stdout)
    }
}
