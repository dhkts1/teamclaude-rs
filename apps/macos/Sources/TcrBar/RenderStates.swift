import AppKit
import SwiftUI
import TcrBarCore

/// Rasterise every panel state to PNG, in-process, then exit.
///
/// ## Why this exists
///
/// Every genuine bug in this app shipped past a green build: a `ScrollView` that
/// collapsed thirteen rows to one, a null quota that blanked the whole panel, and
/// a takeover that reported failure as success. None of them were visible to
/// `swift test`, because none of them are facts about types — they are facts about
/// what AppKit draws once it proposes a size.
///
/// Screen capture is not always available (`screencapture` needs Screen Recording
/// and `osascript` needs assistive access, both of which a build machine or a
/// headless agent may lack), and a screenshot only ever shows the state the live
/// fleet happens to be in. `ImageRenderer` needs neither permission: it draws the
/// real view, with the real tokens, into a bitmap this process owns.
///
/// The states below are chosen because they are the ones that are HARD to observe
/// live — you cannot wait for "every account exhausted" or "a row failed to
/// decode" on demand.
///
/// ## Usage
///
///     TcrBar.app/Contents/MacOS/TcrBar --render-states /tmp/tcrbar-states
///
/// Writes one PNG per state and exits without ever showing a menu-bar item,
/// polling `tcr`, or touching a server.
enum RenderStates {
    static let flag = "--render-states"

    /// Returns the output directory when the process was launched to render.
    static func requestedDirectory(_ arguments: [String] = CommandLine.arguments) -> URL? {
        guard let i = arguments.firstIndex(of: flag), i + 1 < arguments.count else { return nil }
        return URL(fileURLWithPath: arguments[i + 1])
    }

    /// Every state worth looking at, with the name its PNG gets.
    ///
    /// `awake` is a per-scene flag rather than a twelfth state because it is
    /// orthogonal to the poll: the mode can be on under any fleet at all. One
    /// scene carries it, and what that scene reviews is narrower than "the ON
    /// appearance" — it is the caveat line under the checkbox, and the footer
    /// moving down to make room for it. Nothing more:
    ///
    ///  - The tinted mark is drawn on the status item (``MenuBarShell``), which
    ///    is not part of this view, so no scene here renders it.
    ///  - The tick is not rendered either. `ImageRenderer` draws a `.checkbox`
    ///    toggle as the same placeholder in both states, which
    ///    `FleetView.keepAwakeToggle` already says.
    ///
    /// Measured rather than assumed: a band diff of `01-healthy-dark` against
    /// `12-keeping-awake-dark` (max channel delta > 8) differs on 85 of the 897
    /// rows they share, in ONE contiguous band, `y=812..896` — the caveat line
    /// and everything the extra line pushes down. Every row above `y=812`,
    /// checkbox included, is pixel-identical. Scene 12 is also 34px taller,
    /// which is that same line and nothing else.
    ///
    /// Those figures move whenever the footer's wording or spacing does; if they
    /// look stale, re-measure rather than trusting them.
    private static var scenes: [(name: String, state: PollState, awake: Bool, control: String?)] {
        [
            ("01-healthy", .loaded(fleet(healthyJSON)), false, nil),
            ("01c-divergent-windows", .loaded(fleet(divergentWindowsJSON)), false, nil),
            ("01d-unmeasured-window-proof", .loaded(fleet(unmeasuredWindowJSON)), false, nil),
            ("02-mixed-thirteen", .loaded(fleet(mixedJSON)), false, nil),
            ("03-zero-capacity", .loaded(fleet(exhaustedJSON)), false, nil),
            ("04-unmeasured-row", .loaded(fleet(unmeasuredJSON)), false, nil),
            ("04b-needs-relogin-row", .loaded(fleet(needsReloginJSON)), false, nil),
            ("04c-probed-then-broken-row", .loaded(fleet(probedThenBrokenJSON)), false, nil),
            ("05-unreadable-row", .loaded(partiallyUnreadableFleet()), false, nil),
            ("06-offline-source", .loaded(fleet(offlineJSON)), false, nil),
            ("07-empty-fleet", .loaded(fleet("[]")), false, nil),
            (
                "08-tool-missing", .toolMissing(searched: ["/usr/local/bin/tcr", "/opt/homebrew/bin/tcr"]),
                false, nil
            ),
            ("09-command-failed", .commandFailed(exitCode: 1, message: "connection refused"), false, nil),
            ("10-undecodable", .undecodable(message: "DecodingError.valueNotFound: quota"), false, nil),
            ("11-pending", .pending, false, nil),
            ("12-keeping-awake", .loaded(fleet(healthyJSON)), true, nil),
            // The control-account row indicator (`FleetView.controlIndicator`) —
            // the ONE piece of this feature `ImageRenderer` can actually draw.
            // `Menu` contents (the gear's "Use as control account" item, its
            // checkmark) never rasterise regardless of state; see this file's
            // own header and `AccountRow.accountActionsMenu`'s doc-comment.
            ("13-control-account", .loaded(fleet(healthyJSON)), false, "alice@example.com"),
        ]
    }

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
        for scene in scenes {
            for appearance in Appearance.allCases {
                attempted += 1
                if render(scene, appearance: appearance, into: directory) { written += 1 }
            }
        }

        print("\nrendered \(written)/\(attempted) images into \(directory.path)")
        exit(written == attempted ? 0 : 1)
    }

    /// Both appearances, because a token that only exists in one of them is how a
    /// light-mode palette ships broken. The first run of this harness rendered
    /// light-mode by default and I would not otherwise have seen it.
    enum Appearance: String, CaseIterable {
        case dark, light

        var nsAppearance: NSAppearance? {
            NSAppearance(named: self == .dark ? .darkAqua : .aqua)
        }
    }

    @MainActor
    private static func render(
        _ scene: (name: String, state: PollState, awake: Bool, control: String?),
        appearance: Appearance,
        into directory: URL
    ) -> Bool {
        // The appearance has to be current for the duration of the rasterisation:
        // every token resolves through NSColor's dynamic provider, which reads the
        // CURRENT appearance, not one baked into the view.
        let previous = NSAppearance.current
        NSAppearance.current = appearance.nsAppearance
        defer { NSAppearance.current = previous }

        // `.inert`, never the real activity: drawing a checkbox in its ON state
        // must not actually stop this machine sleeping. A harness with a side
        // effect on the operator's power settings would be a worse bug than
        // anything it could catch.
        let awake = AwakeController(activity: .inert)
        awake.setOn(scene.awake)

        let view =
            FleetView(
                poller: StatusPoller(pinnedState: scene.state, lastPollAt: referenceDate),
                server: ServerController(),
                loginItem: LoginItem(),
                accounts: AccountController(),
                control: ControlAccountController(pinned: scene.control),
                awake: awake,
                // `startingUpdater: false`: this process was asked for PNGs. A
                // started updater schedules background checks and can put a
                // window on screen, neither of which belongs in a render run.
                updater: Updater(startingUpdater: false),
                groupController: GroupController(),
                viewMode: FleetViewModePreference(),
                startServerAtLaunch: .constant(false),
                snapshotMode: true
            )
            .environment(\.colorScheme, appearance == .dark ? .dark : .light)
            // A FIXED height, not the measured one.
            //
            // The panel sizes itself from a GeometryReader preference, which needs
            // a second layout pass that ImageRenderer does not perform — the first
            // version of this harness produced eleven blank images because
            // `rowsHeight` was still 0 when the bitmap was taken. Proposing a
            // concrete size renders the real content; the scroll area is simply
            // shown at full height instead of clipped.
            .fixedSize()

        let renderer = ImageRenderer(content: view)
        // 2x so the PNG shows what a Retina panel draws — hairlines and 10pt text
        // are exactly where a 1x render would flatter the design.
        renderer.scale = 2
        renderer.proposedSize = .unspecified

        let name = "\(scene.name)-\(appearance.rawValue).png"
        guard let image = renderer.nsImage,
            let tiff = image.tiffRepresentation,
            let rep = NSBitmapImageRep(data: tiff),
            let png = rep.representation(using: .png, properties: [:])
        else {
            FileHandle.standardError.write(Data("render failed: \(name)\n".utf8))
            return false
        }

        let url = directory.appendingPathComponent(name)
        do {
            try png.write(to: url)
            print("  \(name)  \(Int(image.size.width))x\(Int(image.size.height))pt")
            return true
        } catch {
            FileHandle.standardError.write(Data("write failed \(name): \(error)\n".utf8))
            return false
        }
    }

    /// Tall enough that thirteen rows are all visible rather than scrolled. This
    /// is a review artifact, so seeing everything beats fidelity to the clip.
    private static let renderHeight: CGFloat = 900

    // MARK: - Fixtures
    //
    // Fake accounts only. This repository is public and real account addresses
    // never enter it. A fixed reference date keeps renders byte-comparable
    // between runs, so a diff means the UI changed rather than the clock did.

    private static let referenceDate = Date(timeIntervalSince1970: 1_786_000_000)

    private static func fleet(_ json: String) -> Fleet {
        (try? Fleet.decode(Data(json.utf8))) ?? Fleet(accounts: [])
    }

    /// A fleet with a genuinely undecodable row, which is otherwise almost
    /// impossible to observe on demand.
    private static func partiallyUnreadableFleet() -> Fleet {
        let good = fleet(healthyJSON)
        return Fleet(
            accounts: good.accounts,
            unreadable: [
                Fleet.UnreadableRow(
                    index: 2,
                    message: "valueNotFound: expected Double, found null at .quota")
            ]
        )
    }

    private static func account(
        _ name: String,
        quota: String,
        state: String,
        disabled: Bool = false,
        probe: String = "ok",
        held: String = "[]",
        source: String = "live",
        status: String = "active",
        // Per-window overrides, all defaulting to the composite `quota`/`state`
        // — every EXISTING call site keeps rendering exactly the "5h == 7d"
        // fixture it always has. Only `divergentWindowsJSON` below passes
        // these explicitly, to build the ONE scene where the two windows
        // genuinely disagree — the shape every other fixture here cannot
        // exercise and a swapped 5h/7d binding would render identically to
        // the correct one against.
        fiveHour: String? = nil,
        fiveHourState: String? = nil,
        sevenDay: String? = nil,
        sevenDayState: String? = nil
    ) -> String {
        let fh = fiveHour ?? quota
        let fhState = fiveHourState ?? state
        let sd = sevenDay ?? quota
        let sdState = sevenDayState ?? state
        // `fiveHourState`/`sevenDayState` are JSON string fields on the wire
        // ("ok"/"near"/"spent") but the proof fixture for the unmeasured-
        // window overclaim needs to write a genuine JSON `null`, not the
        // string `"null"` — `QuotaState?` decodes the STRING "null" as
        // `.unknown("null")`, a real (if odd) value, which would silently
        // defeat the one scene built to prove a window has NO reading.
        // `quote(_:)` keeps every other call site (a real state word)
        // wrapped in quotes and only passes `null` through bare.
        func quote(_ raw: String) -> String {
            raw == "null" ? "null" : "\"\(raw)\""
        }
        return """
            {"name":"\(name)","priority":0,"status":"\(status)","disabled":\(disabled),
             "quota":\(quota),"quotaState":"\(state)","fiveHour":\(fh),
             "fiveHourState":\(quote(fhState)),"sevenDay":\(sd),"sevenDayState":\(quote(sdState)),
             "sevenDayOi":0.0,"held":\(held),
             "requests":102,"inputTokens":8781926,"outputTokens":31860,
             "cacheReadTokens":7407414,"cacheHitRatio":0.84,"probeStatus":"\(probe)",
             "probeError":null,"lastStreamError":null,"streamErrorCount":0,
             "source":"\(source)","serverSha":"abc1234","serverDirty":false}
            """
    }

    private static let hold =
        #"[{"window":"7d","minutesUntilReset":6498,"resetAtMs":1786406400224}]"#

    private static var healthyJSON: String {
        "[\(account("alice@example.com", quota: "0.12", state: "ok")),"
            + "\(account("bob@example.com", quota: "0.31", state: "ok"))]"
    }

    /// The bug this scene exists to catch: a 7d-red account must not paint
    /// its 5h bar red, and the inverse — a 5h-red account must not paint its
    /// 7d bar red. Every OTHER fixture in this file sets `fiveHour` and
    /// `sevenDay` to the same value as the composite `quota`, so a binding
    /// bug (5h fraction wired to the 7d bar, or both bars reading the same
    /// state) would render byte-identical to the correct code against every
    /// other scene — this is the one scene that can actually distinguish
    /// them. Two rows, each diverging the OTHER way:
    ///
    ///  - `divergent-low-high`: 5h ~8% (green, `ok`) under 7d ~96% (amber,
    ///    `near`) — the top bar must stay green while the bottom is amber.
    ///  - `divergent-high-low`: 5h ~99% (amber, `near`) over 7d ~15% (green,
    ///    `ok`) — the top bar must be amber while the bottom stays green.
    ///
    /// `near`, not `spent`: the server's own rule (`src/manager/snapshot.rs`)
    /// is `>= 1.0 => Exhausted`, `>= threshold => NearLimit`, else `Normal` —
    /// 0.96 and 0.99 are both under 1.0, so the server can never emit
    /// `"spent"` for them. An earlier version of this fixture painted them
    /// `"spent"` anyway, which is a state the real server cannot produce and
    /// would have taught a future reader a threshold that doesn't exist. The
    /// swap this scene exists to catch shows just as clearly at `near`
    /// (amber) against `ok` (green) as it would at `spent` (red).
    private static var divergentWindowsJSON: String {
        let lowHigh = account(
            "divergent-low-high@example.com", quota: "0.96", state: "near",
            fiveHour: "0.08", fiveHourState: "ok",
            sevenDay: "0.96", sevenDayState: "near")
        let highLow = account(
            "divergent-high-low@example.com", quota: "0.99", state: "near",
            fiveHour: "0.99", fiveHourState: "near",
            sevenDay: "0.15", sevenDayState: "ok")
        return "[\(lowHigh),\(highLow)]"
    }

    /// PROOF fixture for the unmeasured-window overclaim: `sevenDay` is
    /// genuinely spent (1.0/"spent") while `fiveHour`/`fiveHourState` are
    /// BOTH absent — the shape `src/quota.rs` produces whenever the 5h
    /// window has not reported yet but the 7d window already has (the two
    /// populate independently from separate response headers). The 5h bar
    /// must render as a NEUTRAL dashed outline (no reading), never inheriting
    /// the 7d window's red — that is the exact overclaim this whole feature
    /// exists to prevent.
    private static var unmeasuredWindowJSON: String {
        let row = account(
            "unmeasured-5h@example.com", quota: "1.0", state: "spent",
            fiveHour: "null", fiveHourState: "null",
            sevenDay: "1.0", sevenDayState: "spent")
        return "[\(row)]"
    }

    /// The shape that broke: thirteen rows, mixed states, one never-probed.
    private static var mixedJSON: String {
        var rows = (1...4).map {
            account("ok-\($0)@example.com", quota: "0.\($0)2", state: "ok")
        }
        rows.append(account("near@example.com", quota: "0.94", state: "near", held: hold))
        rows += (1...6).map {
            account("spent-\($0)@example.com", quota: "1.0", state: "spent", held: hold)
        }
        rows.append(
            account(
                "never@example.com", quota: "null", state: "ok",
                disabled: true, probe: "never"))
        rows.append(account("parked@example.com", quota: "0.2", state: "ok", disabled: true))
        return "[\(rows.joined(separator: ","))]"
    }

    private static var exhaustedJSON: String {
        "[\((1...3).map { account("spent-\($0)@example.com", quota: "1.0", state: "spent", held: hold) }.joined(separator: ","))]"
    }

    private static var unmeasuredJSON: String {
        "[\(account("alice@example.com", quota: "0.12", state: "ok")),"
            + "\(account("never@example.com", quota: "null", state: "ok", probe: "never"))]"
    }

    private static var offlineJSON: String {
        "[\(account("alice@example.com", quota: "0.12", state: "ok", source: "offline"))]"
    }

    /// The bug this whole scene set exists for: a dead-credential account
    /// (`status:"error"`, `probeStatus:"never"`) beside a healthy one, so the
    /// pill, the status word's tint, the row order and the header clause are
    /// all reviewable together. The Re-login button draws as a placeholder —
    /// `ImageRenderer` cannot rasterise AppKit controls — but its presence
    /// beside Disable still shows in the row's width.
    private static var needsReloginJSON: String {
        let alice = account("alice@example.com", quota: "0.12", state: "ok")
        let dave = account(
            "dave@example.com", quota: "null", state: "ok", probe: "never", status: "error")
        return "[\(alice),\(dave)]"
    }

    /// The blind spot an adversarial review found: `04b` above is the OTHER
    /// way an account breaks — never probed, `quota: null`. This is the shape
    /// that actually happens in production: a credential that dies AFTER
    /// being probed keeps its last-learned `quota` and a real `probeStatus`
    /// (`probe_account`, `src/manager/probing.rs:128-139`, early-returns on
    /// an `Error` row instead of clearing anything; `refresh.rs:93-101` sets
    /// only `status`). `carol@example.com` here carries `status:"error"` WITH
    /// `quota:0.12` and `probeStatus:"ok"` — a real prior reading, not an
    /// absent one — sat beside a genuinely healthy account so the header's
    /// arithmetic ("1 of 2 ready · 1 need re-login", not "2 of 2 ready") is
    /// reviewable in the same frame this scene renders.
    private static var probedThenBrokenJSON: String {
        let alice = account("alice@example.com", quota: "0.12", state: "ok")
        let carol = account(
            "carol@example.com", quota: "0.12", state: "ok", probe: "ok", status: "error")
        return "[\(alice),\(carol)]"
    }
}
