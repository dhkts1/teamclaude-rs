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
///     TcrBar.app/Contents/MacOS/TcrBar --render-states <output-directory>
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
    /// orthogonal to the poll: the mode can be on under any fleet at all. Two
    /// scenes carry it ON — one on the Accounts tab, one on Sessions, since
    /// `FleetView.v4FooterAwakeQuit` draws on every tab and a single scene
    /// would only prove the Accounts case. Neither scene reviews the switch's
    /// visual ON state, and that is a measured limitation, not an oversight:
    ///
    ///  - The tinted mark is drawn on the status item (``MenuBarShell``), which
    ///    is not part of this view, so no scene here renders it.
    ///  - The thumb position is not rendered either. `ImageRenderer` draws a
    ///    `.switch` toggle as the same "prohibited" placeholder regardless of
    ///    `isOn` — the same limitation this file already records for a
    ///    `.checkbox` toggle. Measured, not assumed: `01-healthy-auto-dark.png`
    ///    against `12-keeping-awake-auto-dark.png` (same fleet, `awake` the
    ///    only input that differs) has zero pixels over a channel delta of 8,
    ///    same dimensions. What these scenes prove is that the row renders at
    ///    all on each tab, not what it looks like on.
    ///
    /// Those figures move whenever the footer's wording or spacing does; if they
    /// look stale, re-measure rather than trusting them.
    private static var scenes: [(name: String, state: PollState, awake: Bool, control: String?)] {
        [
            ("01-healthy", .loaded(fleet(healthyJSON)), false, nil),
            ("01c-divergent-windows", .loaded(fleet(divergentWindowsJSON)), false, nil),
            ("01d-unmeasured-window-proof", .loaded(fleet(unmeasuredWindowJSON)), false, nil),
            ("01e-plan-labels", .loaded(fleet(planLabelsJSON)), false, nil),
            ("01f-duplicate-email", .loaded(fleet(duplicateEmailJSON)), false, nil),
            // The control account is pinned to the worst row deliberately: the
            // CONTROL pill is one of the six things competing for its width,
            // and a fixture that left it off would not be the row that
            // overflowed.
            (
                "01g-widest-row", .loaded(fleet(widestRowJSON)), false,
                "henry.fitzgerald@example.com"
            ),
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
            // The same switch, on a tab other than Accounts — the footer row
            // is app state, not tab state (`FleetView.v4FooterAwakeQuit`), so
            // one ON scene away from Accounts is what proves that rather than
            // assumes it.
            ("12b-keeping-awake-sessions-tab", .loaded(sessionsTabFleet), true, nil),
            // The control-account row indicator (`FleetView.controlIndicator`) —
            // the ONE piece of this feature `ImageRenderer` can actually draw.
            // `Menu` contents (the gear's "Use as control account" item, its
            // checkmark) never rasterise regardless of state; see this file's
            // own header and `AccountRow.accountActionsMenu`'s doc-comment.
            //
            // `controlAccountJSON`, not `healthyJSON`: this is the one scene
            // rendered at both `.auto` and `.comfortable` (`densityVariantScenes`
            // below), and `.auto` only resolves `.compact` above four accounts
            // (`PanelDensityPreference.comfortableCeiling`). `healthyJSON`'s two
            // rows sit under that line, so `.auto` and a forced `.comfortable`
            // drew the identical picture and the density variant proved
            // nothing.
            ("13-control-account", .loaded(fleet(controlAccountJSON)), false, "alice@example.com"),
            // Every spend branch at once — see `usageStatsJSON`.
            ("14-usage-stats", .loaded(fleet(usageStatsJSON)), false, nil),
            // A parked group beside a live one — see `parkedGroupJSON`.
            ("15-parked-group", .loaded(fleet(parkedGroupJSON)), false, nil),
            // The SAME fleet, expanded — see `render(_:appearance:into:)`'s
            // own seeding of `FleetView.expandedGroupsKey` for
            // `henry-team`, the wholly-parked group `parkedGroupJSON` builds.
            ("15b-parked-group-expanded", .loaded(fleet(parkedGroupJSON)), false, nil),
            // F2 — the Sessions tab, grouped by account, one row with a
            // tool running and one waiting, plus an unassigned session with
            // no matching Claude Code session file (`sessionsFixture`'s
            // third entry) to review the id-head fallback.
            ("16-sessions-tab", .loaded(sessionsTabFleet), false, nil),
            // F3 — the Tools tab: running now, slowest today (one at the
            // Bash timeout), and the fleet-wide totals line.
            ("17-tools-tab", .loaded(sessionsTabFleet), false, nil),
            // The same fleet with one TIMED OUT TODAY class OPEN — see
            // `expandedTimeoutClassesFixture`. Its own scene because the
            // disclosure is that card's whole point (a count that opens to
            // the commands behind it) and a harness that can only draw it
            // closed reviews half the section.
            ("17b-tools-tab-timeout-class-open", .loaded(sessionsTabFleet), false, nil),
            // The same two tabs at the LIVE fleet's measured dimensions
            // instead of the mockup's — see `realShapeFleet`. These are the
            // only scenes where TIMED OUT TODAY is absent (the real fleet
            // reports no timeout class, so the card does not draw) and the
            // only ones where the Sessions tab's warm-up fold has anything
            // to fold.
            ("22-tools-tab-real-fleet", .loaded(realShapeFleet), false, nil),
            ("22b-sessions-tab-real-fleet", .loaded(realShapeFleet), false, nil),
            // The forward-compat case both tabs must show as one sentence,
            // never an empty list: `healthyJSON` carries no `sessions` key at
            // all, the shape every server shipped before F1.
            ("18-sessions-tab-old-server", .loaded(fleet(healthyJSON)), false, nil),
            // The same server, on the Tools tab. Its own scene because the two
            // tabs draw two different summary lines above that one sentence,
            // and the Tools half is the one the 2026-09-13 interface review
            // caught printing "0 tool calls today" directly over "This server
            // predates sessions — update tcr." (finding 9). A scene nobody
            // renders is a claim nobody can check.
            ("18b-tools-tab-old-server", .loaded(fleet(healthyJSON)), false, nil),
            // The case the single old sentence got WRONG: the proxy is fine
            // and the BUNDLED `tcr` is the stale half, so it has no `sessions`
            // subcommand to ask with. Telling an operator to restart a healthy
            // proxy here is worse than saying nothing.
            ("18c-sessions-tab-old-tcr", .loaded(oldToolFleet), false, nil),
            ("18d-tools-tab-old-tcr", .loaded(oldToolFleet), false, nil),
            // The longest sentence either banner can draw — the CLI's own
            // stderr, inlined. Its own scene because a wrapping failure is
            // only ever visible in pixels.
            ("18e-sessions-tab-command-failed", .loaded(sessionsFailedFleet), false, nil),
            // The Accounts tab's structure, matching
            // the mockup's Accounts panel —
            // 2 solo cards, a 3-member parked group, a 6-member active
            // group — so the pixelmatch gate compares two panels with the
            // same SHAPE, not a 2-card fixture against a 4-section mockup.
            // `01-healthy` is left alone: other scenes and tests key off its
            // exact 2-account shape, and this is a dedicated fixture for the
            // parity gate rather than a rewrite of a scene with other jobs.
            // Pinned so the parity render also proves the `CONTROL` pill in a
            // full panel: `henry10@example.com` is the first loose card, so
            // it draws the pill beside its plan without a group's legend
            // competing for the same row.
            (
                "19-accounts-tab-parity", .loaded(accountsParityFleet), false,
                "henry10@example.com"
            ),
            // The row `v4UpdateRow` restored (FleetView.swift): an available
            // update, drawn under the summary with the "Update…" button —
            // see ``updateState(for:)`` for how the `Updater` gets there.
            ("21-update-available", .loaded(fleet(healthyJSON)), false, nil),
            // Same row, the failure branch — the reason in place of the
            // button, and no button at all.
            ("21b-update-failed", .loaded(fleet(healthyJSON)), false, nil),
            // The Accounts tab's no-requests banner
            // (`FleetView.noRequestsBanner`, `NoRequestsBanner`). A live
            // fleet, every account at zero requests, five minutes in. See
            // ``noRequestsBannerZeroSince`` and ``noRequestsBannerRoute`` for
            // the seeded clock and route.
            ("22-no-requests-banner", .loaded(fleet(zeroRequestsJSON)), false, nil),
        ]
    }

    /// Seeds for scene 22, the one scene that needs
    /// ``FleetView``'s `initialZeroRequestsSince`/`initialClaudeCount`/
    /// `initialClaudeRoute`. Every other scene leaves all three `nil`, which
    /// draws no banner at all, exactly the panel's own default.
    private static let noRequestsBannerZeroSince = Date().addingTimeInterval(-600)
    private static let noRequestsBannerRoute = ClaudeRouteRead.Route(
        url: "http://127.0.0.1:9443", source: "settings.json")

    /// The ``UpdateState`` a scene's `Updater` should report, or `nil` for
    /// every scene not about the update row — the same by-name lookup
    /// ``initialTab(for:)`` already uses rather than a tuple element every
    /// other scene would carry for nothing.
    private static func updateState(for sceneName: String) -> UpdateState? {
        switch sceneName {
        case "21-update-available": return .available(version: "0.2.50")
        case "21b-update-failed":
            return .failed(
                "the proxy on :3456 rejected the api-key in "
                    + "~/.config/teamclaude.json while checking the feed")
        default: return nil
        }
    }

    /// The sign-in sheet (``LoginSheet``), one PNG per state, drawn WHERE IT
    /// APPEARS: over the Accounts tab, on the dimming scrim a real sheet puts
    /// there.
    ///
    /// A separate list because a sheet is not a `PollState`: it is presented
    /// over the panel, and `ImageRenderer` draws a view, never a presentation —
    /// a `.sheet` modifier on the panel rasterises as the panel alone. So the
    /// harness composes the two by hand, which is exactly why ``LoginSheet``
    /// takes a phase rather than a live ``LoginSession``. The composition is
    /// an approximation of AppKit's presentation (which insets and shadows the
    /// sheet itself), and it answers the question a bare sheet could not: how
    /// much of the panel is still legible behind it, and where the eye lands.
    ///
    /// **The spinner is drawn as a still `clock`** (`LoginSheet.snapshotMode`).
    /// `ImageRenderer` rasterises a `ProgressView` as the macOS "prohibited"
    /// placeholder — a red crossed-out circle, the same class of limitation
    /// this file already records for a `.checkbox` toggle — so the fixture
    /// showed a state the app never draws. The still glyph is a stand-in for
    /// motion, and is not evidence about the spinner either way.
    ///
    /// `.failed` is the state this list exists for. It is the one nobody sees
    /// until it happens to them, it carries the longest string on the sheet
    /// (`tcr`'s own refusal, unparaphrased), and a wrapping failure line is the
    /// kind of thing a green build says nothing about.
    private static var sheetScenes: [(name: String, phase: LoginPhase, url: URL?)] {
        let authorize = URL(
            string: "https://claude.ai/oauth/authorize?code=true&state=RENDER-FIXTURE")
        return [
            ("20-login-opening", .opening, nil),
            (
                "20b-login-waiting", .waitingForBrowser(email: "alice@example.com"),
                authorize
            ),
            ("20c-login-saved", .saved(account: "alice@example.com"), nil),
            (
                "20d-login-failed",
                .failed(
                    reason:
                        "the proxy on :3456 rejected the api-key in "
                        + "~/.config/teamclaude.json while checking whether it could take a "
                        + "live login — no browser was opened and nothing was changed."),
                nil
            ),
        ]
    }

    /// Which tab a scene opens on — `.accounts` for every scene above scene
    /// 16, so this stays a lookup by name rather than a fifth tuple element
    /// every existing scene would have to grow.
    private static func initialTab(for sceneName: String) -> PanelTab {
        switch sceneName {
        case "12b-keeping-awake-sessions-tab", "16-sessions-tab", "18-sessions-tab-old-server",
            "18c-sessions-tab-old-tcr", "18e-sessions-tab-command-failed",
            "22b-sessions-tab-real-fleet":
            return .sessions
        case "17-tools-tab", "17b-tools-tab-timeout-class-open", "18b-tools-tab-old-server",
            "18d-tools-tab-old-tcr", "22-tools-tab-real-fleet":
            return .tools
        default: return .accounts
        }
    }

    /// `SessionFile`s for ``sessionsFixture``'s three sessions, on the two
    /// scenes that render it — found in review (2026-09-12): without
    /// these, `FleetView`'s `snapshotMode` never reads a file for any
    /// session (its own doc-comment: "the harness's session ids are fixture
    /// strings that join to nothing real"), so every session read as
    /// `.unknown` → "idle" regardless of what `sessionsFixture`'s own
    /// comments say each one is doing. The three statuses here match that
    /// fixture's own narrative exactly: `aaaaaaaa` has a Bash call running
    /// now (busy), `bbbbbbbb`'s row comment says "waiting 12m" on the
    /// mockup this fixture is modelled on, `cccccccc` is the idle,
    /// unassigned control case.
    private static func sessionFilesFixture(for sceneName: String) -> [String: SessionFile] {
        // `realShapeFleet`'s two working sessions need files for the same
        // reason the mockup fixture's do: without one a session reads
        // `.unknown` and renders "idle", which on the real-shape scenes would
        // draw a tab where NOTHING is busy and quietly libel the fold. Its 18
        // warm-ups are deliberately given no file — a cache-warm stub really
        // does have no Claude Code session behind it, so `.unknown` is the
        // honest read there, not a gap in the fixture.
        if sceneName == "22-tools-tab-real-fleet" || sceneName == "22b-sessions-tab-real-fleet" {
            return [
                "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa": SessionFile(
                    sessionId: "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa",
                    cwd: "/Users/alice/git/demo", name: "demo-a1", status: "busy"),
                "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb": SessionFile(
                    sessionId: "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb",
                    cwd: "/Users/bob/git/demo-ui", name: "demo-ui-b2", status: "busy"),
            ]
        }
        guard
            sceneName == "16-sessions-tab" || sceneName == "17-tools-tab"
                || sceneName == "17b-tools-tab-timeout-class-open"
        else { return [:] }
        return [
            "aaaaaaaa-1111-2222-3333-444444444444": SessionFile(
                sessionId: "aaaaaaaa-1111-2222-3333-444444444444",
                cwd: "/Users/alice/git/teamclaude-rs", name: "teamclaude-rs-c7",
                status: "busy"),
            "bbbbbbbb-1111-2222-3333-444444444444": SessionFile(
                sessionId: "bbbbbbbb-1111-2222-3333-444444444444",
                cwd: "/Users/alice/git/orchard", name: "orchard-c2",
                status: "waiting"),
            "cccccccc-1111-2222-3333-444444444444": SessionFile(
                sessionId: "cccccccc-1111-2222-3333-444444444444",
                cwd: "/Users/alice/git/orchard coder", name: "m-075377",
                status: "idle"),
            "dddddddd-1111-2222-3333-444444444444": SessionFile(
                sessionId: "dddddddd-1111-2222-3333-444444444444",
                cwd: "/Users/bob/git/toolkit", name: "toolkit-c1",
                status: "busy"),
            "eeeeeeee-1111-2222-3333-444444444444": SessionFile(
                sessionId: "eeeeeeee-1111-2222-3333-444444444444",
                cwd: "/Users/bob/git/token", name: "token-b4",
                status: "idle"),
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
                if render(scene, appearance: appearance, density: .auto, into: directory) {
                    written += 1
                }
            }
            // The parity fixture also renders at `.comfortable`, suffixed —
            // see `densityVariantScenes` — so the mockup comparison stays
            // possible at both densities `PanelDensityPreference` offers,
            // not only the shipped default.
            //
            // The main pass above is `.auto`, the shipped default since Gil's
            // "make compact the default please above 4 accounts": forcing
            // `.compact` there would have made every PNG in this set a
            // picture of a setting nobody has, which is the one thing a
            // review harness may not be.
            if densityVariantScenes.contains(scene.name) {
                for appearance in Appearance.allCases {
                    attempted += 1
                    if render(
                        scene, appearance: appearance, density: .comfortable, into: directory)
                    {
                        written += 1
                    }
                }
            }
        }
        for scene in sheetScenes {
            for appearance in Appearance.allCases {
                attempted += 1
                if renderSheet(scene, appearance: appearance, into: directory) { written += 1 }
            }
        }
        for scene in peerSceneList {
            for appearance in Appearance.allCases {
                attempted += 1
                if renderPeer(scene, appearance: appearance, into: directory) { written += 1 }
            }
        }
        for scene in peerSheetScenes {
            for appearance in Appearance.allCases {
                attempted += 1
                if renderPeerSheet(scene, appearance: appearance, into: directory) { written += 1 }
            }
        }
        for scene in controlScenes {
            for appearance in Appearance.allCases {
                attempted += 1
                if renderControl(scene, appearance: appearance, into: directory) { written += 1 }
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

    /// Scenes rendered twice — once at `V4`'s shipped `.auto` default, once
    /// forced to `.comfortable` — so the one fixture compared against
    /// the mockup crop stays comparable at both densities, not only the one
    /// now shipping. Every
    /// other scene renders `.compact` alone: this harness is a review
    /// artifact, and doubling all 24 scenes would be 24 extra PNGs nobody
    /// asked to review.
    private static let densityVariantScenes: Set<String> = [
        "19-accounts-tab-parity",
        // The `CONTROL` pill is new chrome on the card header. `.comfortable`
        // is the density the shipped default and `.auto` can both resolve
        // to, so rendering both against this one card shows whether the
        // pill still fits beside `ROTATING` and the state pill rather than
        // taking the fit on faith from a single default render.
        "13-control-account",
    ]

    @MainActor
    private static func render(
        _ scene: (name: String, state: PollState, awake: Bool, control: String?),
        appearance: Appearance,
        density: PanelDensity,
        into directory: URL
    ) -> Bool {
        // The appearance has to be current for the duration of the rasterisation:
        // every token resolves through NSColor's dynamic provider, which reads the
        // CURRENT appearance, not one baked into the view.
        return withDrawingAppearance(appearance.nsAppearance) {
            renderUnderCurrentAppearance(
                scene, appearance: appearance, density: density, into: directory)
        }
    }

    /// The body of ``render(_:appearance:density:into:)``, run with the drawing
    /// appearance already installed.
    ///
    /// A separate function only because the macOS 12 replacement for assigning
    /// `NSAppearance.current` takes a block (``withDrawingAppearance(_:perform:)``):
    /// wrapping ninety lines in a closure would have re-indented the whole
    /// function to change nothing.
    @MainActor
    private static func renderUnderCurrentAppearance(
        _ scene: (name: String, state: PollState, awake: Bool, control: String?),
        appearance: Appearance,
        density: PanelDensity,
        into directory: URL
    ) -> Bool {
        // `V4.compact` reads this key straight out of `UserDefaults`
        // (`PanelDensityPreference.current()`), so forcing a density for one
        // render is a write-then-restore around this call, the same pattern
        // `expandedGroupsKey` already uses below for scene 15b. Removed
        // rather than restored to a prior value: nothing in this process
        // should be running under a real density preference already set, and
        // "absent" is `PanelDensityPreference`'s own definition of the
        // shipped default.
        UserDefaults.standard.set(density.rawValue, forKey: PanelDensityPreference.key)
        defer { UserDefaults.standard.removeObject(forKey: PanelDensityPreference.key) }

        // `.harness()`, never a real controller: drawing a checkbox in its ON
        // state must not actually stop this machine sleeping. A harness with a
        // side effect on the operator's power settings would be a worse bug than
        // anything it could catch.
        //
        // That sentence used to be satisfied by `activity: .inert` alone, and
        // stopped being once the control started REMEMBERING its state: the
        // `setOn` below would have written `keepThisMacAwake` into the
        // operator's own defaults — true on scene 12, false on the next one —
        // so a render would silently disarm a setting they had turned on.
        // `.harness()` is inert on both halves.
        let awake = AwakeController.harness()
        awake.setOn(scene.awake)

        let updater = Updater(startingUpdater: false)
        if let state = updateState(for: scene.name) {
            updater.setUpdateStateForPreview(state)
        }

        // `expandedGroups` reads `UserDefaults.standard` at construction
        // — real for the shipping app, but this harness only ever runs under
        // `TCRBAR_DEV_BUILD=1`'s OWN bundle id (`build-tcrbar.sh`'s own
        // comment: "gives a non-shipping build its own identity"), a
        // `UserDefaults` domain the shipping app never reads or writes.
        // Seeded here, for exactly one scene, rather than threading a new
        // constructor parameter through `FleetView` for a harness-only need:
        // every OTHER scene explicitly clears the key first, so construction
        // order across scenes cannot leak one render's expansion into the
        // next.
        if scene.name == "15b-parked-group-expanded" {
            UserDefaults.standard.set(
                ["g:henry-team"], forKey: FleetView.expandedGroupsKey)
        } else {
            UserDefaults.standard.removeObject(forKey: FleetView.expandedGroupsKey)
        }

        let view =
            FleetView(
                poller: StatusPoller(pinnedState: scene.state, lastPollAt: referenceDate),
                // The parity scene compares against a mockup whose proxy was
                // running, so its server is pinned to the supervised state:
                // otherwise the app draws "Start server", "Take over port…" and
                // "Not supervised by TcrBar" — three real controls for a state
                // the mockup never had — and the two panels differ by a fact
                // about this machine rather than by a layout decision. Pinned,
                // never spawned: `ServerController.harness(pinned:)` signals
                // nothing.
                server: parityScenes.contains(scene.name)
                    ? ServerController.harness(pinned: .supervising(pid: 4242))
                    : ServerController(),
                loginItem: LoginItem(),
                accounts: AccountController(),
                control: ControlAccountController(pinned: scene.control),
                awake: awake,
                // `startingUpdater: false`: this process was asked for PNGs. A
                // started updater schedules background checks and can put a
                // window on screen, neither of which belongs in a render run.
                // `updateState(for:)` above sets ``Updater/updateState``
                // directly for the two scenes that need it.
                updater: updater,
                groupController: GroupController(),
                removeController: RemoveAccountController(),
                startServerAtLaunch: .constant(false),
                snapshotMode: true,
                initialTab: initialTab(for: scene.name),
                initialSessionFiles: sessionFilesFixture(for: scene.name),
                initialMachineStats: machineFixture,
                initialRunningProcesses: runningProcessesFixture(
                    for: scene.name, state: scene.state),
                initialExpandedTimeoutClasses: expandedTimeoutClassesFixture(for: scene.name),
                initialZeroRequestsSince: scene.name == "22-no-requests-banner"
                    ? noRequestsBannerZeroSince : nil,
                initialClaudeCount: scene.name == "22-no-requests-banner" ? 1 : nil,
                initialClaudeRoute: scene.name == "22-no-requests-banner"
                    ? noRequestsBannerRoute : nil
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

        // The shipped default stays unsuffixed — every existing filename this
        // harness produces is unchanged — and only the forced `.comfortable`
        // variant gets `-comfortable`, per `densityVariantScenes`. Computed
        // here rather than in `rasterise`, which is shared with the sheet
        // scenes and has no density of its own.
        let name =
            density == .compact
            ? "\(scene.name)-\(appearance.rawValue).png"
            : "\(scene.name)-\(density.rawValue)-\(appearance.rawValue).png"
        return rasterise(view, named: name, into: directory)
    }

    /// One state of the sign-in sheet, rendered on its own.
    ///
    /// Same appearance dance as ``render(_:appearance:into:)`` and the same
    /// writer, and deliberately NO controller of any kind: a ``LoginSession``
    /// would spawn `tcr login`. The harness draws a phase, never a login.
    @MainActor
    private static func renderSheet(
        _ scene: (name: String, phase: LoginPhase, url: URL?),
        appearance: Appearance,
        into directory: URL
    ) -> Bool {
        return withDrawingAppearance(appearance.nsAppearance) {
            renderSheetUnderCurrentAppearance(scene, appearance: appearance, into: directory)
        }
    }

    /// The body of ``renderSheet(_:appearance:into:)``, run with the drawing
    /// appearance already installed — the same split, for the same reason, as
    /// ``renderUnderCurrentAppearance(_:appearance:density:into:)``.
    @MainActor
    private static func renderSheetUnderCurrentAppearance(
        _ scene: (name: String, phase: LoginPhase, url: URL?),
        appearance: Appearance,
        into directory: URL
    ) -> Bool {
        // The panel underneath is the ordinary healthy Accounts tab, built the
        // same way every other scene builds one — pinned state, harness
        // controllers, nothing that can spawn or signal anything.
        UserDefaults.standard.removeObject(forKey: FleetView.expandedGroupsKey)
        let panel =
            FleetView(
                poller: StatusPoller(
                    pinnedState: .loaded(fleet(healthyJSON)), lastPollAt: referenceDate),
                server: ServerController.harness(pinned: .supervising(pid: 4242)),
                loginItem: LoginItem(),
                accounts: AccountController(),
                control: ControlAccountController(pinned: nil),
                awake: AwakeController.harness(),
                updater: Updater(startingUpdater: false),
                groupController: GroupController(),
                removeController: RemoveAccountController(),
                startServerAtLaunch: .constant(false),
                snapshotMode: true,
                initialTab: .accounts,
                initialSessionFiles: [:]
            )
            .environment(\.colorScheme, appearance == .dark ? .dark : .light)
            .fixedSize()

        let view =
            panel
            .overlay {
                ZStack {
                    Color.black.opacity(sheetScrimAlpha)
                    // The rounded fill goes UNDER the sheet rather than
                    // clipping it: `.clipShape` + `.shadow` rasterises through
                    // an offscreen layer whose backing showed as white corners
                    // around the light-mode sheet, which is not a surface this
                    // app has.
                    LoginSheet(phase: scene.phase, authorizeURL: scene.url, snapshotMode: true)
                        .background(RoundedRectangle(cornerRadius: V4.cardRadius).fill(Tok.panel))
                        .shadow(radius: sheetShadowRadius)
                }
            }
            .environment(\.colorScheme, appearance == .dark ? .dark : .light)
        return rasterise(view, named: "\(scene.name)-\(appearance.rawValue).png", into: directory)
    }

    /// Harness-only geometry for the sheet composition above: how far the
    /// panel behind a sheet is dimmed, and the sheet's drop shadow. Not design
    /// tokens and not in `V4.swift` — AppKit owns both for a real
    /// presentation, and `V4.swift` holds transcribed mockup values only.
    /// These exist so the fixture reads like the thing it is picturing.
    private static let sheetScrimAlpha: Double = 0.45
    private static let sheetShadowRadius: CGFloat = 12

    // MARK: - The Peers tab

    /// The Peers tab's states, scenes 45 to 51 plus the unsupported collapse
    ///, the Peers tab mockup (kept outside the tree), one scene per state, in
    /// its order.
    ///
    /// A THIRD scene array, because peers are a sibling document
    /// (`tcr peer ls --json`), not a field of
    /// ``PollState``, so a peer state cannot be expressed as one of `scenes`'s
    /// tuples at all. Same shape as ``sheetScenes`` and for the same class of
    /// reason.
    ///
    /// Every fixture below is a ``PeersSnapshot`` built through
    /// ``PeersSnapshotBuilder`` from a ``PeerListDocument``, the real decode
    /// path, with a pinned `now`, so a scene cannot show a sentence the
    /// running panel would not produce from the same JSON. And the controller
    /// is ``PeerController/pinned(_:)``: no subprocess, no listener, no proxy,
    /// nothing that could reach `127.0.0.1:3456`.
    /// Every scene, plus the refusal banner each one draws (almost always
    /// none).
    private static var peerSceneList:
        [(name: String, snapshot: PeersSnapshot, dry: Bool, refusal: PeerRefusal)]
    {
        peerStates.map { ($0.name, $0.snapshot, $0.dry, PeerRefusal()) } + [
            // A verb this panel ran was refused, and the tab is STILL THERE.
            //
            // It had no fixture, which is how the opposite shipped: a refusal
            // used to be drawn instead of the whole tab and then wiped by the
            // next poll about three seconds later. The message is the one a
            // mismatched pairing produces, because that is the longest
            // refusal on this path and the one most likely to be cut.
            (
                "63-peers-refused",
                peersSnapshot(
                    PeerListDocument(
                        finding: true,
                        peers: [
                            .init(
                                name: "studio-mac", address: "studio-mac.local:7749",
                                lastSeenMs: peerMsAgo(2))
                        ])),
                false,
                PeerRefusal(
                    message: "tcr peer share on failed (exit 1): peer share: refused, no Mac "
                        + "is trusted yet, so there is nobody to share with")
            )
        ]
    }

    private static var peerStates: [(name: String, snapshot: PeersSnapshot, dry: Bool)] {
        [
            // 1. A fresh install: finding off, and the only state a one-Mac
            //    network ever has.
            ("45-peers-off", peersSnapshot(PeerListDocument()), false),
            // 2. Looking, and nothing found. A named state rather than a
            //    spinner over an empty list, because "there may be nothing to
            //    find" is the honest answer and `finding` is its own field.
            ("46-peers-finding", peersSnapshot(PeerListDocument(finding: true)), false),
            // 3. Two found, neither trusted, the second with its name NOT
            //    announced (decision row 9), so this side has an address and
            //    no name and the row is drawn rather than hidden.
            (
                "47-peers-found-untrusted",
                peersSnapshot(
                    PeerListDocument(
                        finding: true,
                        peers: [
                            .init(
                                name: "studio-mac", address: "studio-mac.local:7749",
                                lastSeenMs: peerMsAgo(2)),
                            .init(address: "10.0.1.24:7749", lastSeenMs: peerMsAgo(6)),
                        ])),
                false
            ),
            // 4. The safe steady state, and the one most operators stay in:
            //    one Mac trusted and carrying, sharing still off.
            (
                "48-peers-trusted-share-off",
                peersSnapshot(
                    PeerListDocument(
                        finding: true,
                        peers: [
                            .init(
                                id: "tcr-4b8we1r0zp", name: "studio-mac",
                                address: "studio-mac.local:7749", trusted: true,
                                lastSeenMs: peerMsAgo(3), carries: true),
                            .init(
                                name: "attic-nuc", address: "attic-nuc.local:7749",
                                lastSeenMs: peerMsAgo(40)),
                        ])),
                false
            ),
            // 5. The one screen where plaintext crosses a machine boundary, so
            //    the one screen that uses the reserved hue: sharing on, with
            //    the meter that says how much. The second row reads `nothing
            //    yet` rather than a blank, zero spent is a measurement.
            (
                "49-peers-share-on",
                peersSnapshot(
                    PeerListDocument(
                        finding: true, sharing: true,
                        peers: [
                            .init(
                                id: "tcr-4b8we1r0zp", name: "studio-mac",
                                address: "studio-mac.local:7749", trusted: true,
                                lastSeenMs: peerMsAgo(2), carries: true, serves: true,
                                leaseSpent: 0.34, leaseTtlSeconds: 240),
                            .init(
                                id: "tcr-92hbq5t7yv", name: "attic-nuc",
                                address: "attic-nuc.local:7749", trusted: true,
                                lastSeenMs: peerMsAgo(5), carries: true, serves: true,
                                leaseSpent: 0, leaseTtlSeconds: 300),
                        ])),
                false
            ),
            // 6. The other direction, and the reason the mesh exists: this
            //    Mac's own accounts are dry (`dry: true` puts the header's
            //    summary line in its own honest state) and studio-mac is
            //    answering. The egress line is the first thing an operator
            //    reads, because it explains why work is still moving.
            (
                "50-peers-borrowing",
                peersSnapshot(
                    PeerListDocument(
                        finding: true, sharing: true,
                        peers: [
                            .init(
                                id: "tcr-4b8we1r0zp", name: "studio-mac",
                                address: "studio-mac.local:7749", trusted: true,
                                lastSeenMs: peerMsAgo(1), carries: true, serves: true,
                                inFlight: 2, leaseSpent: 0.62, leaseTtlSeconds: 180),
                            .init(
                                id: "tcr-92hbq5t7yv", name: "attic-nuc",
                                address: "attic-nuc.local:7749", trusted: true,
                                lastSeenMs: peerMsAgo(4), carries: true, serves: true,
                                noHeadroom: true),
                        ],
                        answeringOn: .init(peer: "studio-mac", inFlight: 2))),
                true
            ),
            // 7. Sharing on and the peer asleep. The meter reads zero rather
            //    than the 34% it read six minutes ago (the mockup's rule 5),
            //    and the row still says the offer stands, two fields, because
            //    one could not do both.
            (
                "51-peers-asleep",
                peersSnapshot(
                    PeerListDocument(
                        finding: true, sharing: true,
                        peers: [
                            .init(
                                id: "tcr-4b8we1r0zp", name: "studio-mac",
                                address: "studio-mac.local:7749", trusted: true,
                                lastSeenMs: peerMsAgo(360), carries: true, serves: true,
                                leaseSpent: 0.34, leaseTtlSeconds: 0)
                        ])),
                false
            ),
            // 8. A Mac asking to pair, which is decision row 10's own first
            //    screen and had no fixture at all: the knock card is drawn by
            //    `PeersTabV4`, the pane fixture carries a `pending` row, and
            //    the TAB never rendered one. A card nobody renders is a card
            //    nobody reviews, and this one carries three controls and the
            //    only sentence on the tab about what Accept does.
            //
            //    The address is private-range and the instance id is
            //    obviously fake: this repository is public and these PNGs are
            //    review artifacts.
            (
                "52-peers-knock",
                peersSnapshot(
                    PeerListDocument(
                        finding: true,
                        peers: [
                            .init(
                                name: "studio-mac", address: "studio-mac.local:7749",
                                lastSeenMs: peerMsAgo(2))
                        ],
                        pending: [
                            .init(
                                addr: "10.0.1.24", instanceId: "8f2c1ad63b0e4471",
                                proposedName: "loft-mini", wireVersion: 1,
                                firstSeenMs: peerMsAgo(30), lastSeenMs: peerMsAgo(4))
                        ])),
                false
            ),
            // 9. Trust pressed, and the other operator has not answered. The
            //    row state, built through the same overlay the
            //    live controller applies, so the fixture cannot show a row
            //    the panel would not draw.
            (
                "53-peers-waiting",
                PeersSnapshotBuilder.waiting(
                    peersSnapshot(
                        PeerListDocument(
                            finding: true,
                            peers: [
                                .init(
                                    name: "studio-mac", address: "studio-mac.local:7749",
                                    lastSeenMs: peerMsAgo(2)),
                                .init(
                                    name: "attic-nuc", address: "attic-nuc.local:7749",
                                    lastSeenMs: peerMsAgo(9)),
                            ])),
                    knocked: ["studio-mac.local:7749"]),
                false
            ),
            // 10. A lease that ENDED, on both sides of it: the Mac this one
            //     lends to (Re-lend, and the row greys) and the Mac that was
            //     serving this one (no button, because re-lending is its
            //     operator's act). Decision row 13 keeps both rows rather
            //     than deleting them.
            (
                "54-peers-lease-ended",
                peersSnapshot(
                    PeerListDocument(
                        finding: true, sharing: true,
                        peers: [
                            .init(
                                id: "tcr-92hbq5t7yv", name: "attic-nuc",
                                address: "attic-nuc.local:7749", trusted: true,
                                lastSeenMs: peerMsAgo(6),
                                lend: [
                                    .init(
                                        leaseId: "ls-2e77", scope: .group("work"),
                                        window: .week, fraction: 0.20,
                                        until: Int64(peerNow.timeIntervalSince1970) - 1800,
                                        ended: true)
                                ]),
                            .init(
                                id: "tcr-4b8we1r0zp", name: "studio-mac",
                                address: "studio-mac.local:7749", trusted: true,
                                lastSeenMs: peerMsAgo(3), carries: true, serves: true,
                                leaseSpent: 0.34, leaseTtlSeconds: 240,
                                until: Int64(peerNow.timeIntervalSince1970) - 600,
                                ended: true),
                        ])),
                false
            ),
            // 11. The same lease still RUNNING, which is the row decision
            //     row 13's "ends in 1 h" is about: without a fixture the
            //     clause could ship saying nothing and every gate would pass.
            (
                "55-peers-lease-ends-soon",
                peersSnapshot(
                    PeerListDocument(
                        finding: true, sharing: true,
                        peers: [
                            .init(
                                id: "tcr-4b8we1r0zp", name: "studio-mac",
                                address: "studio-mac.local:7749", trusted: true,
                                lastSeenMs: peerMsAgo(1), carries: true, serves: true,
                                inFlight: 2, leaseSpent: 0.62, leaseTtlSeconds: 180,
                                until: Int64(peerNow.timeIntervalSince1970) + 3600)
                        ],
                        answeringOn: .init(peer: "studio-mac", inFlight: 2))),
                true
            ),
            // 9. The live half, which is what the paths block is for: two
            //    Macs, two ways to reach each, and the three states of a
            //    measurement side by side. studio-mac has a measured direct
            //    path and a second endpoint nothing has probed; attic-nuc is
            //    reachable only THROUGH studio-mac, which is the row an
            //    operator has to be able to read at a glance, and its direct
            //    path has measured loss. Rendered so the sub-line stack can be
            //    looked at: the arithmetic that reserves room for it
            //    (`PeerRowModel.rowShape`) is a number, and a number is not a
            //    picture of a row.
            (
                "56-peers-paths",
                peersSnapshot(
                    PeerListDocument(
                        finding: true, sharing: true,
                        peers: [
                            .init(
                                id: "tcr-4b8we1r0zp", name: "studio-mac",
                                address: "192.168.1.24:7749", trusted: true,
                                lastSeenMs: peerMsAgo(2), carries: true, serves: true,
                                leaseSpent: 0.34, leaseTtlSeconds: 240,
                                tokensPerHour: 90_000,
                                paths: [
                                    .init(
                                        endpoint: "192.168.1.24:7749", kind: .direct,
                                        rttMs: 18.5, lossPct: 0,
                                        lastOkMs: peerMsAgo(2)),
                                    .init(endpoint: "10.0.1.24:7749", kind: .direct),
                                ]),
                            .init(
                                id: "tcr-92hbq5t7yv", name: "attic-nuc",
                                address: "10.0.1.31:7749", trusted: true,
                                lastSeenMs: peerMsAgo(9), carries: true,
                                paths: [
                                    .init(
                                        endpoint: "10.0.1.31:7749", kind: .direct,
                                        rttMs: 96, lossPct: 0.02),
                                    .init(
                                        endpoint: "tcr-4b8we1r0zp", kind: .via,
                                        rttMs: 128, lossPct: 0,
                                        lastOkMs: peerMsAgo(30)),
                                ]),
                        ])),
                false
            ),
            // One scene per path state. Two states already had a shape on
            // screen; the third had none at all until now, because a
            // trusted Mac with no paths drew no line and the `asleep` pill
            // carried two facts.
            (
                "w12-path-direct",
                peersSnapshot(
                    PeerListDocument(
                        finding: true, sharing: true,
                        peers: [
                            .init(
                                id: "tcr-4b8we1r0zp", name: "studio-mac",
                                address: "192.168.1.24:7749", trusted: true,
                                lastSeenMs: peerMsAgo(2), carries: true, serves: true,
                                leaseSpent: 0.34, leaseTtlSeconds: 240,
                                paths: [
                                    .init(
                                        endpoint: "192.168.1.24:7749", kind: .direct,
                                        rttMs: 14, lossPct: 0, lastOkMs: peerMsAgo(2))
                                ])
                        ])),
                false
            ),
            // Forwarded and lossy: amber, and the forwarder is named rather
            // than shown as the peer id the wire carries. 6 per cent, which is
            // over the 3 per cent line this scene sets, so the scene checks the
            // rule instead of restating it.
            (
                "w12-path-via",
                peersSnapshot(
                    PeerListDocument(
                        finding: true, sharing: true,
                        peers: [
                            .init(
                                id: "tcr-4b8we1r0zp", name: "studio-mac",
                                address: "192.168.1.24:7749", trusted: true,
                                lastSeenMs: peerMsAgo(4), carries: true, serves: true,
                                leaseSpent: 0.34, leaseTtlSeconds: 240,
                                paths: [
                                    .init(
                                        endpoint: "tcr-92hbq5t7yv", kind: .via,
                                        rttMs: 96, lossPct: 0.06, lastOkMs: peerMsAgo(4))
                                ]),
                            .init(
                                id: "tcr-92hbq5t7yv", name: "loft-mini",
                                address: "10.0.1.31:7749", trusted: true,
                                lastSeenMs: peerMsAgo(3), carries: true,
                                paths: [
                                    .init(
                                        endpoint: "10.0.1.31:7749", kind: .direct,
                                        rttMs: 18, lossPct: 0)
                                ]),
                        ])),
                false
            ),
            // Trusted and awake, but nothing has ever found a way to reach
            // it: the case a firewall eating the port produces. A recent
            // lastSeenMs keeps the freshness read as awake rather than
            // asleep, and an empty paths array is the honest absence rather
            // than a stale reading. This used to share `lastSeenMs` and a
            // spent, ended lease with the asleep row below, which drew the
            // same picture for two different facts.
            (
                "w12-path-none",
                peersSnapshot(
                    PeerListDocument(
                        finding: true, sharing: true,
                        peers: [
                            .init(
                                id: "tcr-4b8we1r0zp", name: "studio-mac",
                                address: "studio-mac.local:7749", trusted: true,
                                lastSeenMs: peerMsAgo(2), carries: true)
                        ])),
                false
            ),
            // Every scene of the mini mesh card is the REAL tab with the
            // real card in it, so the mesh cannot show a shape the panel
            // would not draw from the same document.
            //
            // 5a: one Mac direct and healthy, one carried and lossy.
            (
                "w12-mesh-two-macs",
                peersSnapshot(
                    PeerListDocument(
                        finding: true, sharing: true,
                        peers: [
                            .init(
                                id: "tcr-92hbq5t7yv", name: "attic-nuc",
                                address: "10.0.1.31:7749", trusted: true,
                                lastSeenMs: peerMsAgo(2), carries: true,
                                paths: [
                                    .init(
                                        endpoint: "10.0.1.31:7749", kind: .direct,
                                        rttMs: 16, lossPct: 0.01)
                                ]),
                            .init(
                                id: "tcr-4b8we1r0zp", name: "loft-mini",
                                address: "10.0.1.24:7749", trusted: true,
                                lastSeenMs: peerMsAgo(3), carries: true,
                                paths: [
                                    .init(
                                        endpoint: "tcr-92hbq5t7yv", kind: .via,
                                        rttMs: 58, lossPct: 0.05)
                                ]),
                        ],
                        name: "desk-mac")),
                false
            ),
            // 5b: the direct line to loft-mini is gone and the only way left
            // is through attic-nuc, which is still reached directly itself.
            (
                "w12-mesh-via-only",
                peersSnapshot(
                    PeerListDocument(
                        finding: true, sharing: true,
                        peers: [
                            .init(
                                id: "tcr-92hbq5t7yv", name: "attic-nuc",
                                address: "10.0.1.31:7749", trusted: true,
                                lastSeenMs: peerMsAgo(2), carries: true,
                                paths: [
                                    .init(
                                        endpoint: "10.0.1.31:7749", kind: .direct,
                                        rttMs: 18, lossPct: 0)
                                ]),
                            .init(
                                id: "tcr-4b8we1r0zp", name: "loft-mini",
                                address: "10.0.1.24:7749", trusted: true,
                                lastSeenMs: peerMsAgo(8), carries: true,
                                paths: [
                                    .init(
                                        endpoint: "tcr-92hbq5t7yv", kind: .via,
                                        rttMs: 140, lossPct: 0.04)
                                ]),
                        ],
                        name: "desk-mac")),
                false
            ),
            // 5c: no trusted Macs at all, but one found and not yet trusted,
            // so the empty mesh card is pictured beside a real found row
            // rather than beside nothing at all. One sentence, no ring, and
            // no link to a page with nothing on it. A found peer keeps this
            // scene apart from the plain looking state below, which has no
            // peers of any kind.
            (
                "w12-mesh-empty",
                peersSnapshot(
                    PeerListDocument(
                        finding: true,
                        peers: [
                            .init(
                                name: "studio-mac", address: "studio-mac.local:7749",
                                lastSeenMs: peerMsAgo(2))
                        ],
                        name: "desk-mac")),
                false
            ),
            // 5d: seven trusted, five drawn, two collapsed. Chosen because it
            // needs all four loss colours and both line styles at once, and
            // because it is the fixture `PeerMeshLayoutTests` runs its
            // no-collision gate over.
            (
                "w12-mesh-collapse",
                peersSnapshot(
                    PeerListDocument(
                        finding: true, sharing: true,
                        peers: [
                            .init(
                                id: "tcr-92hbq5t7yv", name: "attic-nuc",
                                address: "10.0.1.31:7749", trusted: true,
                                lastSeenMs: peerMsAgo(2), carries: true,
                                paths: [
                                    .init(
                                        endpoint: "10.0.1.31:7749", kind: .direct,
                                        rttMs: 16, lossPct: 0.01)
                                ]),
                            .init(
                                id: "tcr-4b8we1r0zp", name: "loft-mini",
                                address: "10.0.1.24:7749", trusted: true,
                                lastSeenMs: peerMsAgo(4), carries: true,
                                paths: [
                                    .init(
                                        endpoint: "tcr-92hbq5t7yv", kind: .via,
                                        rttMs: 72, lossPct: 0.06)
                                ]),
                            .init(
                                id: "tcr-7a1c5m9x2k", name: "office-mini",
                                address: "10.0.1.41:7749", trusted: true,
                                lastSeenMs: peerMsAgo(6), carries: true,
                                paths: [
                                    .init(
                                        endpoint: "10.0.1.41:7749", kind: .direct,
                                        rttMs: 210, lossPct: 0.14)
                                ]),
                            .init(
                                id: "tcr-3f8d2v6b1n", name: "lab-mac",
                                address: "10.0.1.52:7749", trusted: true,
                                lastSeenMs: peerMsAgo(600), carries: true),
                            .init(
                                id: "tcr-5k2p8w4r7t", name: "gil-laptop",
                                address: "10.0.1.63:7749", trusted: true,
                                lastSeenMs: peerMsAgo(5), carries: true,
                                paths: [
                                    .init(endpoint: "tcr-92hbq5t7yv", kind: .via, rttMs: 88)
                                ]),
                            .init(
                                id: "tcr-6m3q9z1h5j", name: "shed-mac",
                                address: "10.0.1.74:7749", trusted: true,
                                lastSeenMs: peerMsAgo(7), carries: true,
                                paths: [
                                    .init(
                                        endpoint: "10.0.1.74:7749", kind: .direct,
                                        rttMs: 24, lossPct: 0)
                                ]),
                            .init(
                                id: "tcr-8n4t2y6u0i", name: "van-mac",
                                address: "10.0.1.85:7749", trusted: true,
                                lastSeenMs: peerMsAgo(9), carries: true,
                                paths: [
                                    .init(
                                        endpoint: "10.0.1.85:7749", kind: .direct,
                                        rttMs: 31, lossPct: 0)
                                ]),
                        ],
                        name: "desk-mac")),
                false
            ),
            // The forward-compat state: an older `tcr` answering
            // `{"supported": false}` collapses the tab to ONE honest line and
            // must never draw
            // `Unreadable status output` (`FleetView.swift:1008`). A scene
            // nobody renders is a claim nobody can check.
            (
                "44-peers-unsupported",
                peersSnapshot(PeerListDocument(supported: false)),
                false
            ),
            // A Mac with no network interface at all. There was no fixture
            // for this state, so a Find card arm written for it would have
            // shipped with nobody able to look at it first.
            //
            // Built behind `PeerListDocument` as it stands today, a
            // `finding: false` document with no rows, since a network field
            // has not landed on it yet in this pass. That is the same
            // document `45-peers-off` builds, so this scene draws the same
            // picture as that one until the field exists, a known,
            // deliberate duplicate, not a fixture bug. One line flips it once
            // the field lands: replace `PeerListDocument()` below with
            // `PeerListDocument(network: false)`.
            (
                "64-peers-no-network",
                peersSnapshot(PeerListDocument()),
                false
            ),
        ]
    }

    /// The fixture Settings > Peers renders under `--render-settings`, which
    /// is the Settings > Peers mockup (kept outside the tree)'s own scene 58:
    /// sharing on, two Macs trusted, one asleep, and the five This Mac
    /// readouts present so the pane draws its real values rather than six
    /// "not read yet" rows.
    ///
    /// Lives here, with the other fixtures, rather than in the pane: a
    /// fixture inside a production view is a value that can be shipped by
    /// accident, and every other pinned state this app draws is authored in a
    /// `Render*` file. `PeersSettingsPane`'s own initialiser reaches for it
    /// only when `RenderSettings.requestedDirectory()` says this process was
    /// launched to write PNGs.
    ///
    /// The id and the address are obviously fake, per `CLAUDE.md`: this
    /// repository is public and no real account, host or key goes into a
    /// fixture.
    static var settingsPaneFixture: PeersSnapshot {
        peersSnapshot(
            PeerListDocument(
                finding: true, sharing: true,
                peers: [
                    .init(
                        id: "tcr-4b8we1r0zp", name: "studio-mac",
                        address: "studio-mac.local:7749", trusted: true,
                        lastSeenMs: peerMsAgo(3), carries: true, serves: true,
                        leaseSpent: 0.34, leaseTtlSeconds: 240),
                    .init(
                        id: "tcr-92hbq5t7yv", name: "attic-nuc",
                        address: "attic-nuc.local:7749", trusted: true,
                        lastSeenMs: peerMsAgo(420), carries: true,
                        lend: [
                            .init(
                                leaseId: "ls-4b1f", scope: .group("work"),
                                window: .week, fraction: 0.20, ttlSeconds: 300,
                                maxInFlight: 2,
                                until: Int64(peerNow.timeIntervalSince1970) + 3600),
                            .init(
                                leaseId: "ls-2e77", scope: .accounts(["alice"]),
                                window: .fiveHour, fraction: 0.20, ttlSeconds: 300,
                                maxInFlight: 2,
                                until: Int64(peerNow.timeIntervalSince1970) - 1800,
                                ended: true),
                        ]),
                ],
                name: "studio-mac",
                // Decision row 10 turned this default OFF, and the fixture is
                // the state a fresh config is in: the render then pictures the
                // switch an operator actually meets.
                announceName: false,
                nodeId: "tcr-7f3k9m2q4x",
                listenAddress: "0.0.0.0:7749",
                via: "auto",
                maxHops: 1,
                // The panel-state rows: one Mac knocking, one blocked, one
                // muted, and the caps as `src/main.rs:1202-1208` reports
                // them. Every address is private-range and the labels are
                // sanitized, this repository is public and these PNGs are
                // review artifacts.
                pending: [
                    .init(
                        addr: "10.0.1.24", instanceId: "8f2c1ad63b0e4471",
                        proposedName: "loft-mini", wireVersion: 1,
                        firstSeenMs: peerMsAgo(30), lastSeenMs: peerMsAgo(4))
                ],
                blocked: [
                    .init(
                        addr: "10.0.1.99", sinceMs: peerMsAgo(10800),
                        reason: .forgottenAndBlocked)
                ],
                muted: [.init(addr: "10.0.1.55", untilMs: peerMsAgo(-2820))],
                limited: 3,
                caps: .init(
                    foundRows: 12, foundPerAddress: 2, pending: 8, knockIntervalMs: 10000,
                    knockBurst: 3, unauthenticatedSockets: 16),
                lentTo: [
                    "alice": [
                        .init(
                            peer: "attic-nuc", scope: .group("work"), window: .week,
                            fraction: 0.20),
                        .init(
                            peer: "studio-mac", scope: .accounts(["alice"]), window: .fableWeek,
                            fraction: 1.0),
                    ]
                ]))
    }

    /// Seconds ago, as Unix milliseconds against ``peerNow``, so a rendered
    /// age is the same on every run, unlike the running panel's live clock.
    private static func peerMsAgo(_ seconds: TimeInterval) -> Int64 {
        Int64(peerNow.addingTimeInterval(-seconds).timeIntervalSince1970 * 1000)
    }

    /// The instant every peer scene is rendered AT. Pinned for the reason the
    /// whole harness is pinned: an age that read "2s ago" on one run and
    /// "3s ago" on the next would make every peer PNG differ from the last
    /// one for no design reason.
    ///
    /// Not private: the Settings pane reads it too, under `--render-settings`
    /// and only there. Its lease rows ask the clock what has ended, so a pane
    /// drawn against the REAL clock read every fixture lease as expired the
    /// day this instant fell behind today, and scene 63's "2 running, 1 ended"
    /// rendered as three ended rows.
    static let peerNow = Date(timeIntervalSince1970: 1_786_000_000)

    private static func peersSnapshot(_ document: PeerListDocument) -> PeersSnapshot {
        PeersSnapshotBuilder.snapshot(from: document, now: peerNow)
    }

    /// One Peers-tab state, drawn inside the REAL panel shell.
    ///
    /// ``PanelV4`` takes its content as a closure precisely so a tab can be
    /// composed into it, which is what lets these scenes carry the header, the
    /// summary line and the four-tab strip with Peers selected, the mockup's
    /// own frame, without `FleetView` needing a way to inject fixture peers.
    @MainActor
    private static func renderPeer(
        _ scene: (name: String, snapshot: PeersSnapshot, dry: Bool, refusal: PeerRefusal),
        appearance: Appearance,
        into directory: URL
    ) -> Bool {
        withDrawingAppearance(appearance.nsAppearance) {
            rasterise(
                peersPanel(
                    snapshot: scene.snapshot, dry: scene.dry, refusal: scene.refusal,
                    appearance: appearance),
                named: "\(scene.name)-\(appearance.rawValue).png", into: directory)
        }
    }

    /// The five states of the Trust sheet, over the tab it opens from.
    ///
    /// **This is the surface that shipped unreviewable.** The sheet is where
    /// the whole trust ritual happens and the harness had no fixture for it at
    /// all, which is exactly how a sheet whose Trust button could never be
    /// pressed passed every gate: nobody could look at it. One PNG per state,
    /// both appearances.
    ///
    /// The runs are ``PeerPairRun/init(pinned:)``: no process, no pipe, no
    /// subprocess of any kind, the same door ``PeerController/pinned(_:)``
    /// gives the tab underneath.
    private static var peerSheetScenes: [(name: String, state: PeerPairState, typed: String)] {
        [
            // 1. The knock is away and nobody over there has answered. The
            //    instance id is the argument the OTHER operator types, so the
            //    sheet prints it; this one is obviously fake, as every id in
            //    this file is.
            ("57-trust-waiting", .asking(instance: "8f2c1ad63b0e4471"), ""),
            // 2. The pivotal screen: this Mac's six digits, and an empty field
            //    for the six the other screen is showing. Trust is drawn
            //    disabled here, which is the state an operator meets first.
            ("58-trust-compare", .comparing(code: "418902"), ""),
            // 3. The same screen with the other Mac's digits typed in full, so
            //    the enabled control has a picture too. Without this one the
            //    only rendered Trust button is a dim one, and "the control is
            //    reachable" would again be a claim with no fixture behind it.
            ("59-trust-compare-typed", .comparing(code: "418902"), "418902"),
            // 4. Pinned.
            ("60-trust-done", .done(peer: "tcr-4b8we1r0zp"), ""),
            // 5. Refused, in the CLI's own words. A MISMATCH, which is the
            //    one refusal this path exists to produce.
            (
                "61-trust-refused",
                .refused(
                    "peer pair: refused, 418902 here, 418903 there. A mismatch is the one "
                        + "signal this path exists to produce, so it is not a retry prompt"),
                "418903"
            ),
            // 6. The operator stopped it.
            ("62-trust-cancelled", .cancelled, ""),
        ]
    }

    /// One Trust sheet state, over the found-rows tab it opens from.
    @MainActor
    private static func renderPeerSheet(
        _ scene: (name: String, state: PeerPairState, typed: String),
        appearance: Appearance,
        into directory: URL
    ) -> Bool {
        withDrawingAppearance(appearance.nsAppearance) {
            let run = PeerPairRun(pinned: scene.state)
            run.compare.set(scene.typed)
            let view =
                peersPanel(snapshot: trustSheetTab, dry: false, appearance: appearance)
                .overlay {
                    ZStack {
                        Color.black.opacity(sheetScrimAlpha)
                        PeerTrustSheet(
                            peerName: "studio-mac",
                            state: scene.state,
                            compare: .constant(run.compare),
                            snapshotMode: true
                        )
                        .background(RoundedRectangle(cornerRadius: V4.cardRadius).fill(Tok.panel))
                        .shadow(radius: sheetShadowRadius)
                    }
                }
                .environment(\.colorScheme, appearance == .dark ? .dark : .light)
            return rasterise(
                view, named: "\(scene.name)-\(appearance.rawValue).png", into: directory)
        }
    }

    /// The tab underneath every Trust sheet: the found row the sheet was
    /// opened from, already waiting, which is what the live panel draws.
    private static var trustSheetTab: PeersSnapshot {
        PeersSnapshotBuilder.waiting(
            peersSnapshot(
                PeerListDocument(
                    finding: true,
                    peers: [
                        .init(
                            name: "studio-mac", address: "studio-mac.local:7749",
                            lastSeenMs: peerMsAgo(8))
                    ])),
            knocked: ["studio-mac.local:7749"])
    }

    /// The Peers tab as a panel, for the scene renderers above.
    @MainActor
    private static func peersPanel(
        snapshot: PeersSnapshot, dry: Bool, refusal: PeerRefusal = PeerRefusal(),
        appearance: Appearance
    ) -> some View {
        // Density: absent, which is `PanelDensityPreference`'s own
        // definition of the shipped default, the same write-then-remove
        // every other scene in this file does.
        UserDefaults.standard.removeObject(forKey: PanelDensityPreference.key)
        let scene = (snapshot: snapshot, dry: dry)
        return
            PanelV4(
                    freshness: "updated 2s ago",
                    tabs: PanelTab.allCases,
                    selected: .peers,
                    badges: [:],
                    onSelect: { _ in },
                    onSettings: {},
                    // Thirteen, the fleet every other scene in this file
                    // draws, so `PanelDensity.auto` resolves the same way it
                    // does on the Accounts tab and these PNGs are comparable
                    // with those.
                    accountCount: 13,
                    summary: {
                        SummaryLine(
                            lines: [
                                scene.dry
                                    ? [
                                        .init(text: "13 accounts", tint: Tok.dim),
                                        .init(text: "none with headroom", tint: Tok.near),
                                    ]
                                    : [
                                        .init(text: "13 accounts", tint: Tok.dim),
                                        .init(
                                            text: "6 with headroom", tint: Tok.ok,
                                            emphasised: true),
                                    ]
                            ])
                    },
                    content: {
                        PeersTabV4(
                            controller: PeerController.pinned(scene.snapshot, refusal: refusal),
                            snapshotMode: true)
                    },
                    footer: { EmptyView() }
                )
                .environment(\.colorScheme, appearance == .dark ? .dark : .light)
                // A FIXED size, for the reason the fleet scenes give: the
                // panel sizes itself from a GeometryReader preference and
                // `ImageRenderer` performs no second layout pass.
                .fixedSize()
    }

    // MARK: - Settings > Peers controls

    /// One control, one state, one PNG.
    ///
    /// A FOURTH scene array, and the reason is the same class as
    /// ``peerSceneList``'s: these controls live on the Settings pane, whose own
    /// harness (`RenderSettings`) hosts a real window and can draw exactly ONE
    /// fixture per pane. A control with four states needs four pictures, so
    /// each one is rasterised here on its own, in the panel harness that needs
    /// no window at all.
    ///
    /// What that costs, said rather than hidden: this pictures the CONTROL,
    /// not the pane around it. The pane's own render stays `--render-settings`.
    private struct ControlScene {
        let name: String
        let view: AnyView
    }

    @MainActor
    private static var controlScenes: [ControlScene] {
        [
            // Item 1, the mockup's scenes 1a to 1c plus the transient state
            // the lead ruled in (`wave12-ui-findings.md`, "Lead answers").
            ControlScene(
                name: "w12-internet-off",
                view: AnyView(PeerInternetRow(on: false, state: .off))),
            ControlScene(
                name: "w12-internet-asking",
                view: AnyView(PeerInternetRow(on: true, state: .asking))),
            ControlScene(
                name: "w12-internet-mapped",
                view: AnyView(
                    PeerInternetRow(
                        on: true,
                        state: .reachable(
                            // Documentation range (RFC 5737), like every other
                            // address in this file: the repository is public.
                            address: "203.0.113.44", port: 51413,
                            expires: peerNow.addingTimeInterval(120))))),
            ControlScene(
                name: "w12-internet-silent",
                view: AnyView(PeerInternetRow(on: true, state: .routerSilent))),
            // Item 2, the mockup's scenes 2a to 2c. The third is the one a
            // plain two-option toggle would hide: switched back to serve, and
            // the borrower's old key still winding down on its own clock.
            ControlScene(
                name: "w12-mode-serve",
                view: AnyView(
                    LendModeControl(peer: "studio-mac", mode: .serve, status: nil))),
            ControlScene(
                name: "w12-mode-hand-active",
                view: AnyView(
                    LendModeControl(
                        peer: "studio-mac", mode: .hand,
                        status: PeerLease.handedKeyLine(
                            mode: .hand,
                            handedKeyUntil: Int64(peerNow.timeIntervalSince1970) + 262,
                            peer: "studio-mac", now: peerNow)))),
            // Item 3, the mockup's scenes 3a to 3d, on the real account card
            // rather than the row alone: 3d's "waiting for studio-mac" is a
            // PILL in the card header, so a picture of the row on its own
            // could not show the state the lead ruled in.
            ControlScene(
                name: "w12-exits-local",
                view: (exitsCard(.init(route: .local)))),
            ControlScene(
                name: "w12-exits-peer-soft",
                view: (exitsCard(.init(route: .via(exitPeerId))))),
            ControlScene(
                name: "w12-exits-peer-must",
                view: (exitsCard(.init(route: .via(exitPeerId), strict: true)))),
            ControlScene(
                name: "w12-exits-peer-must-waiting",
                view: exitsCard(
                    .init(
                        route: .via(exitPeerId), strict: true, peerDown: true,
                        waitingSeconds: 40))),
            ControlScene(
                name: "w12-mode-hand-winding",
                view: AnyView(
                    LendModeControl(
                        peer: "studio-mac", mode: .serve,
                        status: PeerLease.handedKeyLine(
                            mode: .serve,
                            handedKeyUntil: Int64(peerNow.timeIntervalSince1970) + 166,
                            peer: "studio-mac", now: peerNow)))),
        ]
    }

    /// One account card in one exit state, at the panel's own width.
    ///
    /// The card is the real ``AccountCard`` over the real `alice` fixture, so
    /// a scene cannot show a card the panel would not draw; only the exit
    /// value differs between the four.
    /// The wire id an exit lock actually stores, and the row that turns it
    /// back into a name.
    ///
    /// The scenes used to pin to the string `studio-mac`, which no peers file
    /// ever holds: `egress` is `via <52-character id>`. Drawing the name
    /// without the join made a picture of a case that cannot happen and hid
    /// the one that does, a picker showing an id nobody can read.
    private static let exitPeerId = String(repeating: "0", count: 52)

    private static var exitPeerRows: [PeerListDocument.PeerEntry] {
        [PeerListDocument.PeerEntry(id: exitPeerId, name: "studio-mac", trusted: true)]
    }

    @MainActor
    private static func exitsCard(_ exit: AccountExit) -> AnyView {
        // The first row of the healthy fleet, which is `alice`. An empty fleet
        // would mean this file's own fixture stopped decoding, so the scene
        // draws nothing rather than a card invented here.
        guard let account = fleet(healthyJSON).accounts.first else {
            return AnyView(EmptyView())
        }
        return AnyView(
            AccountCard(
                account: account, shape: .full, now: peerNow,
                exit: exit, exitPeers: ["studio-mac", "attic-nuc"],
                exitPeerRows: exitPeerRows, snapshotMode: true,
                actions: { EmptyView() }
            )
            .frame(width: V4.panelWidth))
    }

    /// One control scene, drawn on the pane's own card fill at the pane's own
    /// width.
    @MainActor
    private static func renderControl(
        _ scene: ControlScene, appearance: Appearance, into directory: URL
    ) -> Bool {
        withDrawingAppearance(appearance.nsAppearance) {
            let view =
                scene.view
                .padding(14)
                // 460 pt: the detail column of the Settings window, which is
                // the width these controls really get (`settings-peers-short`
                // and `PeerPaneLayout`).
                .frame(width: 460, alignment: .leading)
                .background(Tok.cardFill)
                .environment(\.colorScheme, appearance == .dark ? .dark : .light)
                .fixedSize(horizontal: false, vertical: true)

            return rasterise(
                view, named: "\(scene.name)-\(appearance.rawValue).png", into: directory)
        }
    }

    /// Rasterise one view to a PNG under `directory`. The single writer for
    /// every scene in this file, so a panel render and a sheet render cannot
    /// drift onto two different scales.
    @MainActor
    private static func rasterise<V: View>(_ view: V, named name: String, into directory: URL)
        -> Bool
    {
        let renderer = ImageRenderer(content: view)
        // 2x so the PNG shows what a Retina panel draws — hairlines and 10pt text
        // are exactly where a 1x render would flatter the design.
        renderer.scale = 2
        renderer.proposedSize = .unspecified

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

    /// `wait` open on the one scene that reviews the disclosure — the
    /// biggest class, and the one `sessionsFixture` gives commands to from
    /// both of its tool-carrying sessions.
    private static func expandedTimeoutClassesFixture(for sceneName: String) -> Set<String> {
        sceneName == "17b-tools-tab-timeout-class-open" ? ["wait"] : []
    }

    /// One running Bash row's process reading, fixed — the mockup's own
    /// `640% · 2.1G`. Never ``ProcessTable/read()``: the harness must
    /// draw the same pixels on a quiet laptop and on a box mid-build, the
    /// same rule ``machineFixture`` states for the machine line.
    ///
    /// The key can only be derived from THIS scene's own fleet, not written
    /// down: ``SessionToolEntry/id`` contains the call's `startedMs`, and
    /// `sessionsFixture` stamps those off the real clock so its ages read
    /// like a live fleet's.
    ///
    /// Exactly ONE of the two running Bash calls is seeded, on purpose. The
    /// other renders the case that is just as common live and twice as easy
    /// to get wrong: a call this build matched no process to, which draws no
    /// cpu/memory clause and NO ✕ at all. A fixture that seeded both would
    /// leave the refusal unreviewed.
    private static func runningProcessesFixture(for sceneName: String, state: PollState)
        -> [String: RunningCallStats]
    {
        guard
            sceneName == "17-tools-tab" || sceneName == "17b-tools-tab-timeout-class-open",
            case .loaded(let fleet) = state,
            let entry = fleet.toolsRunning.first(where: { $0.call.tool == "Bash" })
        else { return [:] }
        return [
            entry.id: RunningCallStats(
                pid: 48765,
                processGroup: 48765,
                cpuSeconds: 1_294.7,
                residentBytes: 2_254_857_830,
                cpuPercent: 640,
                readAt: referenceDate)
        ]
    }

    /// The machine line's numbers, fixed — `docs/design/tools-tab.md`'s own
    /// line, verbatim. Never ``MachineStats/read()``: a render must produce
    /// the same pixels on a quiet laptop and on a box running five compiles,
    /// or the harness is comparing this machine's load rather than the
    /// panel's layout.
    ///
    /// The tint here is `calm`, not amber: 7.1 on 14 cores is half a load
    /// unit per core, and this build tints by Gil's dispatch rule (amber from
    /// 1x, red past 2x) rather than by the mockup's own `warn` class on this
    /// same line, which the rule contradicts. Measured on the render, not
    /// read off the code — a pixel scan for `#ffd16b` finds amber in the four
    /// TIMED OUT bars and nowhere on the machine line. The bands themselves
    /// are pinned by `MachineStatsTests.testLoadTintBands`, which is where a
    /// threshold belongs; a fixture chosen to picture one band would have
    /// cost this scene the mockup's own numbers, and scene 17 is compared
    /// against the mockup crop.
    private static let machineFixture = MachineStats(
        loadAverage: 7.1,
        cores: 14,
        memoryUsedBytes: 48 * 1_073_741_824,
        memoryTotalBytes: 64 * 1_073_741_824,
        compiles: 5,
        diskFreeBytes: 210_000_000_000)

    /// Tall enough that thirteen rows are all visible rather than scrolled. This
    /// is a review artifact, so seeing everything beats fidelity to the clip.
    private static let renderHeight: CGFloat = 900

    /// The three scenes that are compared against the mockup crops. Each is
    /// rendered with a SUPERVISED server for the reason
    /// `ServerController.harness(pinned:)` states: the mockup's proxy was
    /// running, and a panel drawing "Start server" and "Take over port…" differs
    /// from it by a fact about this machine rather than by a layout decision.
    private static let parityScenes: Set<String> = [
        "19-accounts-tab-parity", "16-sessions-tab", "17-tools-tab",
    ]

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
        sevenDayState: String? = nil,
        // Minutes from NOW until each window resets, written onto the wire as
        // epoch milliseconds the way `tcr` sends them.
        //
        // Relative rather than a fixed timestamp because
        // `QuotaFormat.resetCaption` returns nil for a reset that is not in the
        // future: a hard-coded epoch draws a caption today and nothing a month
        // from now, deleting the thing these scenes exist to show. The cost is
        // that the caption's digits differ between renders.
        fiveHourResetInMinutes: Int? = nil,
        sevenDayResetInMinutes: Int? = nil,
        // The FABLE weekly window — a third window with its own reset, gating
        // Fable requests only. Independent of the two above on purpose: a row
        // whose 7d window is spent can have Fable headroom and the reverse, and
        // a fixture that tied them together could not show it.
        //
        // NOT MEASURED by default — an older server, or a window never learned
        // for this account, which is the majority of a real fleet: the window
        // is only learned once a Fable request has been served on that account.
        // It must draw an EMPTY slot, never `n/a` and never `0%`, so every
        // scene that does not opt in is also the negative case. Scenes 01 and
        // 14 opt in.
        sevenDayOi: String = "null",
        // `"null"` is the shape a proxy predating `sevenDayOiState` sends: a
        // real fraction with no state word beside it, which draws the figure in
        // the neutral tint rather than borrowing the composite state. Scene 14
        // carries one of those too.
        sevenDayOiState: String = "null",
        sevenDayOiResetInMinutes: Int? = nil,
        // The `usage` object, as raw JSON. Defaults to the measured shape, so
        // every existing scene shows the spend line the panel now draws.
        // `"null"` is the not-measured row — an older proxy, or an offline
        // read — and it must render as an EMPTY slot, never as `$0.00`; see
        // `unmeasuredUsage` and scene `14-usage-stats`.
        usage: String = measuredUsage,
        // Group labels, wire fields `"groups"`/`"reservedGroups"`
        // (`FleetStatus.swift:933-953`). `nil` omits both keys entirely, the
        // shape every existing call site keeps decoding — see
        // `Account.groups`'s own doc-comment on why a missing key and an
        // empty array are kept distinct rather than collapsed.
        groups: [String]? = nil,
        reservedGroups: [String]? = nil,
        // The parked subset, wire field `"parkedGroups"`. Rides with `groups`
        // exactly as `reservedGroups` does, so a scene that passes no `groups`
        // is also the negative case for this key.
        parkedGroups: [String]? = nil,
        // Fleet-wide group colours, wire field `"groupColors"`, repeated per
        // row the way the server sends them. `nil` omits the key — the
        // older-server shape, which every pre-existing scene keeps, and which
        // draws every tag in the neutral fallback.
        groupColors: [String: String]? = nil,
        // The plan label the SERVER derived, wire field `"plan"`. `nil` omits
        // the key entirely — the never-profiled row and the older-server row
        // alike, both of which must draw NO tag rather than a guessed one, so
        // every scene that does not opt in is also the negative case.
        plan: String? = nil,
        // The org this row belongs to, wire field `"orgUuid"`. It is what makes
        // two rows sharing a name distinct: `Account.id` is org-qualified, so a
        // fixture pair with one email and two orgs renders as TWO rows here and
        // collapsed to one before that fix. `nil` omits the key, which is the
        // older-server shape.
        orgUuid: String? = nil,
        gate: String? = nil,
        // Requests served since this proxy started, wire field `"requests"`.
        // Every existing call site keeps the measured 102 it always had;
        // only the no-requests banner scene passes 0, which is the whole
        // fact ``NoRequestsBanner`` reads off this field.
        requests: Int = 102
    ) -> String {
        func resetAtMs(_ minutes: Int?) -> String {
            guard let minutes else { return "null" }
            let at = Date().addingTimeInterval(Double(minutes) * 60)
            return "\(Int64(at.timeIntervalSince1970 * 1000))"
        }
        func jsonArray(_ values: [String]) -> String {
            "[" + values.map { "\"\($0)\"" }.joined(separator: ",") + "]"
        }
        let groupsFragment =
            groups.map { g in
                "\"groups\":\(jsonArray(g)),\"reservedGroups\":\(jsonArray(reservedGroups ?? [])),"
                    + "\"parkedGroups\":\(jsonArray(parkedGroups ?? [])),"
            } ?? ""
        let colorsFragment =
            groupColors.map { colors in
                let pairs =
                    colors
                    .sorted { $0.key < $1.key }
                    .map { "\"\($0.key)\":\"\($0.value)\"" }
                    .joined(separator: ",")
                return "\"groupColors\":{\(pairs)},"
            } ?? ""
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
        let planFragment = plan.map { "\"plan\":\"\($0)\"," } ?? ""
        let orgFragment = orgUuid.map { "\"orgUuid\":\"\($0)\"," } ?? ""
        let gateFragment = gate.map { "\"gate\":\"\($0)\"," } ?? ""
        return """
            {"name":"\(name)","priority":0,"status":"\(status)","disabled":\(disabled),
             \(planFragment)\(orgFragment)\(gateFragment)
             "quota":\(quota),"quotaState":"\(state)","fiveHour":\(fh),
             "fiveHourState":\(quote(fhState)),"sevenDay":\(sd),"sevenDayState":\(quote(sdState)),
             "sevenDayOi":\(sevenDayOi),"sevenDayOiState":\(quote(sevenDayOiState)),
             \(groupsFragment)\(colorsFragment)"held":\(held),
             "fiveHourResetAtMs":\(resetAtMs(fiveHourResetInMinutes)),
             "sevenDayResetAtMs":\(resetAtMs(sevenDayResetInMinutes)),
             "sevenDayOiResetAtMs":\(resetAtMs(sevenDayOiResetInMinutes)),
             "requests":\(requests),"inputTokens":8781926,"outputTokens":31860,
             "cacheReadTokens":7407414,"cacheCreationTokens":\(usage == "null" ? "null" : "1200000"),
             "cacheHitRatio":0.84,"probeStatus":"\(probe)",
             "probeError":null,"lastStreamError":null,"streamErrorCount":0,
             "source":"\(source)","serverSha":"abc1234","serverDirty":false,
             "usage":\(usage)}
            """
    }

    // MARK: Usage fixtures
    //
    // The numbers RECONCILE with the row-level counters above, because a
    // fixture that does not is a fixture that teaches a reader a wrong
    // relationship: the row's `inputTokens` is the QUOTA counter and folds all
    // three input dimensions together, so base input (174512) + cache creation
    // (1200000) + cache reads (7407414) == 8781926, and 7407414/8781926 is the
    // 0.84 `cacheHitRatio` already on the row. The per-model buckets sum to
    // `today` on every field, the same way the server's own do.
    //
    // `cacheCreation1hTokens` is a SUBSET of `cacheCreationTokens`, never an
    // addend — 400000 of today's 1200000 written at the long TTL — which is
    // what the server sends and what `UsageTotals.cacheHitRatio` divides by. A
    // fixture carrying 0 there could not tell the two readings apart.

    /// A fully measured, fully priced account: two models, a named quota
    /// window, and nothing unpriced.
    private static let measuredUsage = """
        {"today":{"requests":102,"inputTokens":174512,"cacheCreationTokens":1200000,
          "cacheCreation1hTokens":400000,"cacheReadTokens":7407414,"outputTokens":31860,
          "costUsd":14.1657,"unpricedRequests":0},
         "window":{"requests":40,"inputTokens":68000,"cacheCreationTokens":471000,
          "cacheCreation1hTokens":157000,"cacheReadTokens":2900000,"outputTokens":12476,
          "costUsd":5.6141,"unpricedRequests":0,"since":1767207600000},
         "lastHour":{"requests":12,"inputTokens":20000,"cacheCreationTokens":141000,
          "cacheCreation1hTokens":47000,"cacheReadTokens":705000,"outputTokens":3756,
          "costUsd":1.6413,"unpricedRequests":0},
         "todayByModel":{
           "claude-opus-5":{"requests":70,"inputTokens":122158,"cacheCreationTokens":840000,
            "cacheCreation1hTokens":280000,"cacheReadTokens":5185190,"outputTokens":21140,
            "costUsd":12.0785,"unpricedRequests":0},
           "claude-sonnet-5":{"requests":32,"inputTokens":52354,"cacheCreationTokens":360000,
            "cacheCreation1hTokens":120000,"cacheReadTokens":2222224,"outputTokens":10720,
            "costUsd":2.0872,"unpricedRequests":0}}}
        """

    /// The same measured shape as ``measuredUsage`` with the three figures the
    /// panel actually PRINTS dialled to a caller's numbers: today's spend (a
    /// collapsed group's "· $8.42 today"), and the window's spend and output
    /// tokens (an account card's "$540 · 1.5M out").
    ///
    /// Built as a format, not by rewriting `measuredUsage`'s text: a
    /// string-replace on a shared JSON literal would hit whichever bucket
    /// happened to carry the same digits, and the parity fixtures below need
    /// exactly these three to move and the rest to stay put.
    private static func measuredUsage(
        todayCost: Double, windowCost: Double, windowOutputTokens: Int
    ) -> String {
        """
        {"today":{"requests":102,"inputTokens":174512,"cacheCreationTokens":1200000,
          "cacheCreation1hTokens":400000,"cacheReadTokens":7407414,"outputTokens":31860,
          "costUsd":\(todayCost),"unpricedRequests":0},
         "window":{"requests":40,"inputTokens":68000,"cacheCreationTokens":471000,
          "cacheCreation1hTokens":157000,"cacheReadTokens":2900000,
          "outputTokens":\(windowOutputTokens),
          "costUsd":\(windowCost),"unpricedRequests":0,"since":1767207600000},
         "lastHour":{"requests":12,"inputTokens":20000,"cacheCreationTokens":141000,
          "cacheCreation1hTokens":47000,"cacheReadTokens":705000,"outputTokens":3756,
          "costUsd":1.6413,"unpricedRequests":0},
         "todayByModel":{
           "claude-opus-5":{"requests":70,"inputTokens":122158,"cacheCreationTokens":840000,
            "cacheCreation1hTokens":280000,"cacheReadTokens":5185190,"outputTokens":21140,
            "costUsd":12.0785,"unpricedRequests":0},
           "claude-sonnet-5":{"requests":32,"inputTokens":52354,"cacheCreationTokens":360000,
            "cacheCreation1hTokens":120000,"cacheReadTokens":2222224,"outputTokens":10720,
            "costUsd":2.0872,"unpricedRequests":0}}}
        """
    }

    /// Nothing this account served could be priced: `costUsd` is null in every
    /// bucket and `unpricedRequests` says how many requests are missing from
    /// the figure. The card must print the token count ALONE — no `$0.00` —
    /// and the header must append `N unpriced`.
    private static let unpricedUsage = """
        {"today":{"requests":102,"inputTokens":174512,"cacheCreationTokens":1200000,
          "cacheCreation1hTokens":400000,"cacheReadTokens":7407414,"outputTokens":31860,
          "costUsd":null,"unpricedRequests":102},
         "window":{"requests":40,"inputTokens":68000,"cacheCreationTokens":471000,
          "cacheCreation1hTokens":157000,"cacheReadTokens":2900000,"outputTokens":12476,
          "costUsd":null,"unpricedRequests":40,"since":1767207600000},
         "lastHour":{"requests":12,"inputTokens":20000,"cacheCreationTokens":141000,
          "cacheCreation1hTokens":47000,"cacheReadTokens":705000,"outputTokens":3756,
          "costUsd":null,"unpricedRequests":12},
         "todayByModel":{
           "claude-sonnet-4-5-20250929":{"requests":102,"inputTokens":174512,
            "cacheCreationTokens":1200000,"cacheCreation1hTokens":400000,
            "cacheReadTokens":7407414,"outputTokens":31860,"costUsd":null,
            "unpricedRequests":102}}}
        """

    /// Measured and priced, but the server cannot name when this account's
    /// 5-hour window started, so `window` is null and the card falls back to
    /// the DAY's figures — which are still a measurement.
    ///
    /// Its day is split across FOUR priced models, which is what carries the
    /// header line's own case. It was one model until the line stopped
    /// collapsing its tail to `"+N"`; with two labels in the whole fleet, the
    /// scene built to review that line could not show what it now does. Four
    /// here plus the unpriced `sonnet-4-5` the other two rows carry gives the
    /// header five entries — one more than the live fleet this was measured
    /// against ran on 2026-08-30 — so the scene reviews the wrap at a width
    /// past the ordinary case rather than short of it.
    ///
    /// `claude-haiku-4-5` is $0.0102 of $22.25 on purpose: 0.05% of the fleet's
    /// day, which is the slice that rounds to zero. It draws `haiku-4-5 <1%`,
    /// and a scene showing `haiku-4-5 0%` is the regression — a model that
    /// served 2 requests reported as having spent nothing. See
    /// ``QuotaFormat/share(_:)``.
    ///
    /// The buckets still sum to `today` on every field, the way the rest of
    /// these fixtures do: 60+25+15+2 requests, and $6.90 + $2.00 + $0.50 +
    /// $0.0102 = the $9.4102 above.
    private static let noWindowUsage = """
        {"today":{"requests":102,"inputTokens":174512,"cacheCreationTokens":1200000,
          "cacheCreation1hTokens":400000,"cacheReadTokens":7407414,"outputTokens":31860,
          "costUsd":9.4102,"unpricedRequests":0},
         "window":null,
         "lastHour":{"requests":12,"inputTokens":20000,"cacheCreationTokens":141000,
          "cacheCreation1hTokens":47000,"cacheReadTokens":705000,"outputTokens":3756,
          "costUsd":1.1021,"unpricedRequests":0},
         "todayByModel":{
           "claude-opus-5":{"requests":60,"inputTokens":100000,
            "cacheCreationTokens":700000,"cacheCreation1hTokens":230000,
            "cacheReadTokens":4400000,"outputTokens":18000,"costUsd":6.9,
            "unpricedRequests":0},
           "claude-fable-5":{"requests":25,"inputTokens":45000,
            "cacheCreationTokens":300000,"cacheCreation1hTokens":100000,
            "cacheReadTokens":1900000,"outputTokens":8000,"costUsd":2.0,
            "unpricedRequests":0},
           "claude-sonnet-5":{"requests":15,"inputTokens":27000,
            "cacheCreationTokens":190000,"cacheCreation1hTokens":65000,
            "cacheReadTokens":1050000,"outputTokens":5300,"costUsd":0.5,
            "unpricedRequests":0},
           "claude-haiku-4-5-20251001":{"requests":2,"inputTokens":2512,
            "cacheCreationTokens":10000,"cacheCreation1hTokens":5000,
            "cacheReadTokens":57414,"outputTokens":560,"costUsd":0.0102,
            "unpricedRequests":0}}}
        """

    /// Priced, but not all of it: 12 of today's 102 requests ran on a model
    /// this build has no rate for, so `costUsd` is a FLOOR and
    /// `unpricedRequests` says by how much. The card must print `$5.61+` —
    /// the fleet header's `N unpriced` clause is computed from `today` and
    /// cannot speak for one account's window.
    ///
    /// Its `todayByModel` carries both the priced model and the unpriced one,
    /// summing to `today` the way the server's own buckets do, so the header
    /// has a model whose share is genuinely unknown to render as `?`.
    private static let partiallyPricedUsage = """
        {"today":{"requests":102,"inputTokens":174512,"cacheCreationTokens":1200000,
          "cacheCreation1hTokens":400000,"cacheReadTokens":7407414,"outputTokens":31860,
          "costUsd":12.8402,"unpricedRequests":12},
         "window":{"requests":40,"inputTokens":68000,"cacheCreationTokens":471000,
          "cacheCreation1hTokens":157000,"cacheReadTokens":2900000,"outputTokens":12476,
          "costUsd":5.6141,"unpricedRequests":12,"since":1767207600000},
         "lastHour":{"requests":12,"inputTokens":20000,"cacheCreationTokens":141000,
          "cacheCreation1hTokens":47000,"cacheReadTokens":705000,"outputTokens":3756,
          "costUsd":1.6413,"unpricedRequests":0},
         "todayByModel":{
           "claude-opus-5":{"requests":90,"inputTokens":154512,"cacheCreationTokens":1100000,
            "cacheCreation1hTokens":360000,"cacheReadTokens":6907414,"outputTokens":27860,
            "costUsd":12.8402,"unpricedRequests":0},
           "claude-sonnet-4-5-20250929":{"requests":12,"inputTokens":20000,
            "cacheCreationTokens":100000,"cacheCreation1hTokens":40000,
            "cacheReadTokens":500000,"outputTokens":4000,"costUsd":null,
            "unpricedRequests":12}}}
        """

    /// Not measured: an offline read, or a proxy built before `usage` existed.
    /// The 5h slot stays empty and the header line is not drawn at all.
    private static let unmeasuredUsage = "null"

    private static let hold =
        #"[{"window":"7d","minutesUntilReset":6498,"resetAtMs":1786406400224}]"#

    /// Two healthy accounts, and the row's two shapes side by side: alice has a
    /// reset on every window and carries a caption beside each percentage; bob
    /// has none on the wire and must still read as a complete row. The caption
    /// is drawn when there is one, never reserved as blank space.
    ///
    /// Both carry a Fable weekly reading, which is what the README's own shot is
    /// cut from — the two shapes of that slot, a percentage with its countdown
    /// and a percentage alone, in the panel a reader sees first. Neither is
    /// `near` or `spent`: this is the healthy scene, and the two tinted shapes
    /// are scene 14's job.
    private static var healthyJSON: String {
        "[\(account("alice@example.com", quota: "0.12", state: "ok", fiveHourResetInMinutes: 130, sevenDayResetInMinutes: 4_320, sevenDayOi: "0.21", sevenDayOiState: "ok", sevenDayOiResetInMinutes: 6_498, plan: "Max 20x", orgUuid: "11111111-1111-1111-1111-111111111111")),"
            + "\(account("bob@example.com", quota: "0.31", state: "ok", sevenDayOi: "0.44", sevenDayOiState: "ok", groups: ["research"], reservedGroups: ["research"], plan: "Team 5x", orgUuid: "22222222-2222-2222-2222-222222222222"))]"
    }

    /// Five plain accounts, `alice@example.com` first: scene 13's own fleet.
    ///
    /// Five, not `healthyJSON`'s two: `.auto` only resolves `.compact` above
    /// ``PanelDensityPreference/comfortableCeiling`` (four) accounts, and
    /// scene 13 is the one scene rendered at both `.auto` and a forced
    /// `.comfortable`. A fleet at or under the ceiling draws the identical
    /// picture either way, which is why the two density variants used to be
    /// indistinguishable.
    private static var controlAccountJSON: String {
        "["
            + (1...5).map { i in
                i == 1
                    ? account(
                        "alice@example.com", quota: "0.12", state: "ok", plan: "Max 20x",
                        orgUuid: "11111111-1111-1111-1111-111111111111")
                    : account("member\(i)@example.com", quota: "0.\(i)0", state: "ok")
            }.joined(separator: ",") + "]"
    }

    /// Scene 22: a single account, a LIVE read (so
    /// ``NoRequestsBanner/totalRequests(_:)`` sees a measured zero rather than
    /// an offline `nil`), and zero requests served: the colleague's fleet
    /// this feature exists for.
    private static var zeroRequestsJSON: String {
        "[\(account("colleague@example.com", quota: "0.0", state: "ok", source: "live", plan: "Team Standard", orgUuid: "33333333-3333-3333-3333-333333333333", requests: 0))]"
    }

    /// F1's wire shape, attached to
    /// `alice`'s row the way the server sends it — see ``Account/sessions``'s
    /// doc-comment for why nothing here is wired to a live fetch yet: this
    /// build has no safe channel for real session data, so the review
    /// fixture builds ``Session`` values directly rather than decoding them
    /// off `tcr status --json`'s bare account array, the same way
    /// `FleetStatusTests` now does. Scenes 16 and 17 both use this Fleet;
    /// only ``initialTab(for:)`` decides which tab opens.
    ///
    /// The mockup's own five sessions (`docs/design/panel-tabs-mockup.html`'s
    /// Sessions panel), verbatim: `teamclaude-rs-c7` (busy, two running
    /// tools — a Bash call and the Agent call that is 20s from the 600s
    /// timeout) and `orchard-c2` (waiting 12m) under `henry10@example.com`;
    /// `m-075377` (idle 40m) rounds out that account's three; `toolkit-c1`
    /// (busy, one running Bash call) and `token-b4` (idle 2h) are
    /// `henry1@example.com`'s two. Panel-parity round: previously this
    /// fixture held 3 sessions on 1 account, none matching the mockup's
    /// names, models or per-row metrics — this round matches all five
    /// exactly (requests, cache%, the running-tool ages) so the tab compares
    /// layout and styling, not five wrong numbers.
    ///
    /// The mockup's own summary line ("12 sessions · 7 busy · 1 waiting · 4
    /// idle") and its "Show 7 more sessions" / "3 accounts have no sessions"
    /// disclosure are NOT reproduced here: this build has no disclosure
    /// feature (`FleetView.sessionsList` renders every session it is given,
    /// unclipped — `snapshotMode`'s own doc-comment says so on purpose), so
    /// seven more fixture sessions would render as seven more full cards the
    /// mockup does not have, which would widen the diff this round exists to
    /// close rather than shrink it. Recorded in `product-wave-findings.md`
    /// as the next round's prerequisite, the same call the previous round
    /// made about the Accounts tab's own disclosure gap.
    private static var sessionsFixture: [Session] {
        // `lastSeenMs`/`firstSeenMs` are epoch milliseconds, and the age
        // label reads real wall-clock `Date()` (`FleetView.trailingStatus`,
        // the same "views re-render often enough" idiom
        // `HeldWindow.countdownLabel` already uses) — not this file's fixed
        // `referenceDate`, which only pins the POLL timestamp shown in the
        // header. These are minutes-ago offsets from the real clock so the
        // rendered age reads like a live fleet's.
        func msAgo(_ seconds: TimeInterval) -> Int64 {
            Int64(Date().addingTimeInterval(-seconds).timeIntervalSince1970 * 1000)
        }
        return [
            // "412 req · cache 97% · 2 running · oldest 9m 40s" — the Agent
            // call (started 9m40s/580s ago) is older than the Bash one
            // (4m12s/252s ago), so it is what `oldest` reads; 580s is 20s
            // short of the 600s Bash timeout, matching the mockup's "20s to
            // timeout" on the Tools tab's RUNNING NOW row for this same call.
            Session(
                sessionId: "aaaaaaaa-1111-2222-3333-444444444444", account: "henry10@example.com",
                model: "claude-fable-5", firstSeenMs: msAgo(3 * 3600), lastSeenMs: msAgo(3 * 60),
                requests: 410, inputTokens: 30_000, outputTokens: 41_000, cacheReadTokens: 970_000,
                tools: SessionTools(
                    // `calls` is the sum of `byTool` below (15,000 + 200 +
                    // 2,000) — the two must agree, per `panel-tabs-review.md`
                    // finding 3: the headline IS the total, never one
                    // category standing in for it.
                    // 23 = this session's own `timeoutsByClass` below
                    // (8 + 9 + 6), and with the sibling's 8 the summary line's
                    // "31 hit the 600s timeout" IS the TIMED OUT TODAY card's
                    // own total. The two numbers are the same fact; a fixture
                    // that let them disagree would render a panel contradicting
                    // itself two lines apart.
                    calls: 17_000, errors: 1, timeouts: 23,
                    running: [
                        ToolCall(
                            tool: "Bash", commandHead: "cargo test --release > test.log",
                            startedMs: msAgo(4 * 60 + 12)),
                        ToolCall(
                            tool: "Agent",
                            commandHead: "Agent · F7 prove time-to-reset value",
                            startedMs: msAgo(9 * 60 + 40)),
                    ],
                    slowest: [
                        ToolCall(
                            tool: "Bash",
                            commandHead: "git -C ~/src/example push > push.log",
                            endedMs: msAgo(5 * 60), seconds: 47.5)
                    ],
                    overOneMinute: 3,
                    // Combined with the sibling session's below, sums to the
                    // mockup's exact BY TOOL numbers: Bash 19,913, Agent 412,
                    // Read·Grep·Edit 4,352 — 24,677 total.
                    byTool: [
                        ToolBucketRow(tool: "Bash", calls: 15_000, secondsP50: 2.0),
                        ToolBucketRow(tool: "Agent", calls: 200, secondsP50: 380),
                        ToolBucketRow(tool: "Read", calls: 2_000, secondsP50: 0.2),
                    ],
                    // The mockup's own TIMED OUT TODAY card: wait 12, build 9,
                    // git-net 4, other 6 — 31, which is the number this tab's
                    // summary line already says hit the 600s timeout. Split
                    // across the two sessions that carry tool data, so the
                    // render also proves the fleet-wide sum rather than one
                    // session's dictionary drawn straight through.
                    timeoutsByClass: ["wait": 8, "build": 9, "other": 6],
                    timedOut: [
                        ToolCall(
                            tool: "Bash",
                            commandHead: "until grep -q \"Ready in\" /tmp/dev.log; do sleep 1; done",
                            commandClass: "wait",
                            endedMs: msAgo(18 * 60), seconds: 600),
                        ToolCall(
                            tool: "Bash",
                            commandHead: "cargo build --release --all-features",
                            commandClass: "build",
                            endedMs: msAgo(52 * 60), seconds: 600),
                    ]),
                // Rising, per the mockup's own aria-label on this session's spark:
                // "Requests per minute over the last 30 minutes: rising".
                // "$4.12" — `SessionRow::cost_usd`, wire 2.
                reqPerMinute: [
                    2, 3, 2, 4, 3, 5, 4, 6, 5, 7, 6, 8, 7, 9, 8,
                    10, 9, 11, 10, 12, 11, 13, 12, 14, 13, 15, 14, 16, 15, 17,
                ],
                costUsd: 4.10),
            // "5,756 req · cache 94% · waiting 12m".
            Session(
                sessionId: "bbbbbbbb-1111-2222-3333-444444444444", account: "henry10@example.com",
                model: "claude-opus-5", firstSeenMs: msAgo(6 * 3600), lastSeenMs: msAgo(12 * 60),
                requests: 5800, inputTokens: 60_000, outputTokens: 88_000, cacheReadTokens: 940_000,
                tools: SessionTools(
                    // 8 = `wait` 4 + `git-net` 4 below.
                    calls: 7_500, errors: 4, timeouts: 8,
                    // Five, the mockup's own count for SLOWEST TODAY — and the
                    // reason there are five: `Fleet.toolsSlowest` pools ten
                    // across every session and the tab draws the top five, so a
                    // fixture with two rows cannot tell "the cap works" apart
                    // from "there was nothing to cap". The seconds are the
                    // mockup's: one at the 600 s timeout, then 583, 556, 343.
                    slowest: [
                        ToolCall(
                            tool: "Bash",
                            commandHead: "/opt/homebrew/bin/bash disk-scan.sh 2>&1 | tee",
                            endedMs: msAgo(11 * 60), seconds: 600.0),
                        ToolCall(
                            tool: "Bash",
                            commandHead: "bash retro-review-wait.sh --until green",
                            endedMs: msAgo(23 * 60), seconds: 583.0),
                        ToolCall(
                            tool: "Bash",
                            commandHead: "until grep -qE \"^(error|warning)\" build.log",
                            endedMs: msAgo(36 * 60), seconds: 556.0),
                        ToolCall(
                            tool: "Bash",
                            // Deliberately longer than the row: the overlap
                            // this fixture exists to catch only appears once
                            // the label cannot fit beside the pill, and every
                            // earlier fixture command fitted.
                            commandHead:
                                "cd ~/src/example && ./scripts/cargo-q.sh test -p teamclaude "
                                + "--all-features 2>&1 | tee test.log",
                            endedMs: msAgo(48 * 60), seconds: 343.0),
                    ],
                    overOneMinute: 5,
                    byTool: [
                        ToolBucketRow(tool: "Bash", calls: 4_900, secondsP50: 2.3),
                        ToolBucketRow(tool: "Agent", calls: 210, secondsP50: 400),
                        ToolBucketRow(tool: "Grep", calls: 2_400, secondsP50: 0.2),
                    ],
                    // The other four of the mockup's `wait` twelve, and its
                    // `git-net` four. A class with a count and no commands is
                    // deliberate here too: `git-net` opens to nothing, which
                    // is the row shape a server sending counts alone draws.
                    timeoutsByClass: ["wait": 4, "git-net": 4],
                    timedOut: [
                        ToolCall(
                            tool: "Bash",
                            commandHead: "until [ -f /tmp/merge-gate.done ]; do sleep 5; done",
                            commandClass: "wait",
                            endedMs: msAgo(31 * 60), seconds: 600)
                    ]),
                // Falling, per the mockup's aria-label on this session's spark.
                // "$5.29"; with the sibling above, the account block's header
                // reads the mockup's "3 sessions · $9.41".
                reqPerMinute: [
                    15, 14, 13, 12, 11, 10, 9, 8, 7, 6, 5, 5, 4, 4, 3,
                    3, 3, 2, 2, 2, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0,
                ],
                costUsd: 5.30),
            // "idle 40m" — no metrics line at all in the mockup, and
            // `FleetView.sessionRow`'s compact idle layout now matches that.
            Session(
                sessionId: "cccccccc-1111-2222-3333-444444444444", account: "henry10@example.com",
                model: "claude-sonnet-5", firstSeenMs: msAgo(45 * 60), lastSeenMs: msAgo(40 * 60),
                requests: 3, inputTokens: 900, outputTokens: 80, cacheReadTokens: 600),
            // "6,479 req · cache 95% · 1 running · 1m 03s".
            Session(
                sessionId: "dddddddd-1111-2222-3333-444444444444", account: "henry1@example.com",
                model: "claude-opus-5", firstSeenMs: msAgo(4 * 3600), lastSeenMs: msAgo(63),
                requests: 6500, inputTokens: 50_000, outputTokens: 63_000, cacheReadTokens: 950_000,
                // `calls` (and `byTool`) deliberately left at their zero
                // default: this session's requests (6,479) are a wire fact
                // independent of tool-call volume, and the Tools tab's
                // headline is the sum of every session's `tools.calls` —
                // `panel-tabs-review.md` finding 3's own bug, reproduced
                // here once already this round by a first draft that set
                // `calls: 6_500` with no matching `byTool` entries and
                // pushed the headline to 31,156 against a BY TOOL section
                // still summing to the mockup's 24,677. Only the running
                // call below is this session's contribution to the Tools
                // tab.
                tools: SessionTools(
                    running: [
                        ToolCall(
                            tool: "Bash",
                            commandHead: "swift build -c release --product TcrBar",
                            startedMs: msAgo(63))
                    ]),
                // "$7.90"; with `token-b4`'s $0.12 below, this account's
                // block header reads the mockup's "2 sessions · $8.02".
                reqPerMinute: [
                    4, 5, 4, 6, 5, 7, 6, 8, 7, 9, 8, 10, 9, 11, 10,
                    12, 11, 13, 12, 14, 13, 15, 14, 16, 15, 17, 16, 18, 17, 19,
                ],
                costUsd: 7.90),
            // "idle 2h" — compact layout, same as `m-075377` above.
            Session(
                sessionId: "eeeeeeee-1111-2222-3333-444444444444", account: "henry1@example.com",
                model: "claude-opus-5", firstSeenMs: msAgo(5 * 3600), lastSeenMs: msAgo(2 * 3600),
                requests: 5, inputTokens: 1500, outputTokens: 120, cacheReadTokens: 900,
                // Priced, and deliberately never DRAWN: an idle row is one
                // line with no metrics, so this figure only ever reaches the
                // account block's own total. `m-075377` above is left
                // unpriced (`nil`) so the same block also carries the
                // server-did-not-send case.
                costUsd: 0.12),
        ]
    }

    /// 2 solo cards + a 5-member parked group (`henry-token`) + a 6-member
    /// active group (`orchard`) — the mockup's own account count and group
    /// shape (`docs/design/panel-tabs-mockup.html`'s Accounts panel), built
    /// entirely from the existing `account()`/group machinery.
    ///
    /// The counts are the point, not decoration: five parked accounts is what
    /// puts two of them behind `FleetView`'s "Show 2 more accounts" button,
    /// and six live ones is what trips ``FleetSection/collapsesByDefault``, so
    /// this scene is the render-harness proof that both disclosure shapes
    /// draw. The spend figures are dialled so the collapsed group's summary
    /// line reads the mockup's own "6 accounts · $8.42 today".
    private static var accountsParityJSON: String {
        // "$540 · 1.5M out" and "$1,190 · 3.1M out" on the two solo cards'
        // plan lines, the mockup's own figures.
        // `henry10` carries a Fable weekly window and `henry5` below does not,
        // so this one scene shows both halves of the rule the card follows: a
        // third `fable` row when ``Account/sevenDayOi`` is present, and NO row
        // at all when it is absent. An empty `fable` track on an account with
        // no such window would claim a window that does not exist, and a
        // fixture where every row has one could not tell the two apart.
        let solo1 = account(
            "henry10@example.com", quota: "0.07", state: "ok", sevenDay: "0.30",
            sevenDayState: "ok",
            fiveHourResetInMinutes: 182, sevenDayResetInMinutes: 6_540,
            sevenDayOi: "0.71", sevenDayOiState: "near", sevenDayOiResetInMinutes: 6_498,
            usage: measuredUsage(
                todayCost: 540.12, windowCost: 540.12, windowOutputTokens: 1_500_000),
            plan: "Max 20x", orgUuid: "11111111-1111-1111-1111-111111111111")
        // `near`, not `warn`: the wire's only three quota-state tokens are
        // `ok`/`near`/`spent` (`src/cli.rs`'s `quota_state_token`), and the
        // composite is the most-spent of the two windows — so an account whose
        // 7d window sits at 98 % reports `near` on BOTH. The fixture used to
        // send `warn`, a token no `tcr` emits: it decoded to `.unknown`, which
        // painted the 98 % bar the unmeasured violet and left the card's pill
        // reading OK — the very defect the parity pass was opened to fix,
        // reproduced by the fixture rather than by the panel.
        let solo2 = account(
            "henry5@example.com", quota: "0.98", state: "near",
            fiveHour: "0.04", fiveHourState: "ok",
            sevenDay: "0.98", sevenDayState: "near",
            fiveHourResetInMinutes: 182, sevenDayResetInMinutes: 5_640,
            usage: measuredUsage(
                todayCost: 1_190.4, windowCost: 1_190.4, windowOutputTokens: 3_100_000),
            plan: "Max 20x", orgUuid: "22222222-2222-2222-2222-222222222222")
        let tokenColors = ["henry-token": "#92d188", "orchard": "#c79ae8"]
        // Five parked members — three drawn, two behind the button.
        let parkedPlans = ["Team 5x", "Team Standard", "Team 5x", "Team Standard", "Team 5x"]
        // Row two is the UNMEASURED case, and unmeasured means NO READING —
        // `quota: "null"`, the shape `tcr` sends for an account nothing has
        // been learned about. It used to say `("0.0", "unmeasured")`: a real
        // zero reading with an invented state word, which rendered as the
        // `UNKNOWN` pill (a state this build cannot name) where the mockup
        // draws `UNMEASURED` (no state to name yet). Opposite meanings.
        let parkedStates = [
            ("0.10", "ok"), ("null", "ok"), ("0.55", "near"),
            ("0.22", "ok"), ("0.31", "ok"),
        ]
        let parkedNames = [
            "gil@example.com", "henry1@example.com", "henry2@example.com",
            "henry3@example.com", "henry4@example.com",
        ]
        let tokenRows = (0..<5).map { i in
            account(
                parkedNames[i], quota: parkedStates[i].0, state: parkedStates[i].1,
                groups: ["henry-token"], parkedGroups: ["henry-token"], groupColors: tokenColors,
                plan: parkedPlans[i], orgUuid: "33333333-3333-3333-3333-333333333333")
        }
        // 5 x $1.40 + $1.42 = $8.42, the mockup's own collapsed-group total.
        let orchardRows = (1...6).map { i in
            account(
                "orchard\(i)@example.com", quota: i == 6 ? "0.60" : "0.15",
                state: i == 6 ? "near" : "ok",
                usage: measuredUsage(
                    todayCost: i == 6 ? 1.42 : 1.40, windowCost: 0.9,
                    windowOutputTokens: 12_476),
                groups: ["orchard"], groupColors: tokenColors,
                plan: "Team Standard", orgUuid: "44444444-4444-4444-4444-444444444444")
        }
        let all = [solo1, solo2] + tokenRows + orchardRows
        return "[\(all.joined(separator: ","))]"
    }

    /// The parity scene's fleet: ``accountsParityJSON``'s accounts PLUS a
    /// session list, because the strip's badges are shared chrome and the
    /// mockup's Accounts panel draws both of them (`Sessions 12`, `Tools 3`).
    /// Rendered from a fleet with no sessions, the two badges vanish and the
    /// parity comparison silently loses the delta it was meant to prove.
    ///
    /// ``sessionsFixture``'s five sessions carry all three running tool calls,
    /// so `Tools` reads 3 with no help. `Sessions` needs the mockup's twelve:
    /// the seven added here are quiet rows — no tools, no sparkline — which is
    /// the only thing a COUNT needs them to be, and they are never drawn on
    /// this tab.
    private static var accountsParityFleet: Fleet {
        let base = fleet(accountsParityJSON)
        let quiet = (1...7).map { index in
            Session(
                sessionId: "f000000\(index)-1111-2222-3333-444444444444",
                account: "henry10@example.com", model: "claude-sonnet-5",
                firstSeenMs: Int64(Date().addingTimeInterval(-3600).timeIntervalSince1970 * 1000),
                lastSeenMs: Int64(
                    Date().addingTimeInterval(-Double(index) * 300).timeIntervalSince1970 * 1000),
                requests: 4, inputTokens: 900, outputTokens: 80, cacheReadTokens: 600)
        }
        return Fleet(
            accounts: base.accounts, unreadable: base.unreadable,
            sessions: sessionsFixture + quiet, sessionsSupported: true)
    }

    /// A healthy fleet whose sessions channel failed because the BUNDLED `tcr`
    /// has no `sessions` subcommand — see ``Fleet/SessionsChannel``.
    private static var oldToolFleet: Fleet {
        let base = fleet(healthyJSON)
        return Fleet(
            accounts: base.accounts, unreadable: base.unreadable,
            sessionsChannel: .toolPredatesSessions)
    }

    /// The same fleet with the channel's longest sentence: `tcr sessions`
    /// exited non-zero and its stderr rides into the banner.
    private static var sessionsFailedFleet: Fleet {
        let base = fleet(healthyJSON)
        return Fleet(
            accounts: base.accounts, unreadable: base.unreadable,
            sessionsChannel: .commandFailed(
                "could not read live status from the proxy on :3456 (connection refused)"))
    }

    private static var sessionsTabFleet: Fleet {
        let base = fleet(
            "[\(account("henry10@example.com", quota: "0.12", state: "ok", plan: "Max 20x", orgUuid: "11111111-1111-1111-1111-111111111111")),"
                + "\(account("henry1@example.com", quota: "0.31", state: "ok", plan: "Team Standard", orgUuid: "22222222-2222-2222-2222-222222222222"))]"
        )
        return Fleet(
            accounts: base.accounts, unreadable: base.unreadable, sessions: sessionsFixture,
            sessionsSupported: true)
    }

    /// A fleet at the LIVE fleet's measured dimensions rather than the
    /// mockup's, with every identity invented.
    ///
    /// Why a SECOND Tools/Sessions fixture exists. ``sessionsTabFleet`` is the
    /// MOCKUP's fleet, and until now it was the only reader this panel's
    /// design ever had. Measured against the live proxy on 2026-09-17 with
    /// `scripts/fleet-shape.py`, the two had drifted this far apart:
    ///
    ///     dimension          mockup fixture     live
    ///     running calls                   3       68
    ///     sessions                        5       31
    ///     warm-up sessions                0       18
    ///     timeouts today                 31        0
    ///
    /// The last row hid an entire card. TIMED OUT TODAY draws only when the
    /// server reports a timeout class, so on the real fleet it does not draw
    /// at all, and nobody reviewing the mockup fixture had ever seen the tab
    /// without it. This scene has no timeouts for exactly that reason: it is
    /// the Tools tab as the operator actually meets it.
    ///
    /// It also pins the running list's ORDER. `busy-1` below holds a three
    /// hour `Agent` and a `Bash` call fifty seconds from the 600s timeout;
    /// ``Fleet/toolsRunning`` must put the `Bash` on top, because only a
    /// ``ToolCall/capped`` call has a deadline to be near. Sorted by age
    /// alone, the `Agent` would lead and the call about to be killed would sit
    /// under the five-row fold.
    ///
    /// Everything here is SHAPE, never DATA. This repository is public, so the
    /// accounts, session ids, project names and commands are all invented;
    /// only the COUNTS come from the measurement. When the fleet changes,
    /// re-run `scripts/fleet-shape.py` and compare it against the table above
    /// rather than trusting that this still resembles anything.
    /// Every figure below is INVENTED and deliberately round. This fixture
    /// exists to render the panel at the MAGNITUDES a busy fleet produces, and
    /// a magnitude is all it needs: whether a cost fits its column does not
    /// depend on the cost being anyone's real one.
    ///
    /// The first version of this fixture was filled in by reading the live
    /// proxy, which put an operator's session costs, request counts and token
    /// volumes into a public repository wearing fake names. Names, emails,
    /// UUIDs and paths were anonymised; the economics were not, and an
    /// operator's spend is the more sensitive half. If a future change wants
    /// these numbers to look more realistic, invent more realistic numbers.
    /// Do not read them off a running fleet.
    private static var realShapeFleet: Fleet {
        func msAgo(_ seconds: TimeInterval) -> Int64 {
            Int64(Date().addingTimeInterval(-seconds).timeIntervalSince1970 * 1000)
        }
        let base = fleet(
            "[\(account("alice@example.com", quota: "0.95", state: "near", plan: "Max 20x", orgUuid: "33333333-3333-3333-3333-333333333333")),"
                + "\(account("bob@example.com", quota: "0.40", state: "ok", plan: "Max 20x", orgUuid: "44444444-4444-4444-4444-444444444444"))]"
        )
        // The four running calls whose ORDER is the point. Declared here
        // oldest-first on purpose: if the sort ever regresses to age, this
        // fixture renders in exactly this order and the scene shows it.
        let busy = Session(
            sessionId: "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa", account: "alice@example.com",
            model: "claude-opus-5", firstSeenMs: msAgo(5 * 3600), lastSeenMs: msAgo(20),
            requests: 1_500, inputTokens: 250_000, outputTokens: 180_000,
            cacheReadTokens: 300_000_000,
            tools: SessionTools(
                calls: 4_000, errors: 90, timeouts: 0,
                running: [
                    ToolCall(
                        tool: "Agent", commandHead: "explorer: map the retry paths",
                        startedMs: msAgo(3 * 3600)),
                    ToolCall(
                        tool: "Write", commandHead: "Write /Users/alice/git/demo/report.md",
                        startedMs: msAgo(40 * 60)),
                    ToolCall(
                        tool: "Bash", commandHead: "cargo test --all --release > suite.log",
                        commandClass: "build", startedMs: msAgo(9 * 60 + 50)),
                    ToolCall(
                        tool: "Bash", commandHead: "git -C ~/src/demo fetch --all",
                        commandClass: "git-net", startedMs: msAgo(4 * 60 + 12)),
                ],
                slowest: [
                    ToolCall(
                        tool: "Bash", commandHead: "swift build -c release", commandClass: "build",
                        endedMs: msAgo(11 * 60), seconds: 90.0)
                ],
                overOneMinute: 25,
                byTool: [
                    ToolBucketRow(tool: "Bash", calls: 3_300, secondsP50: 3.0),
                    ToolBucketRow(tool: "Read", calls: 320, secondsP50: 2.0),
                    ToolBucketRow(tool: "Write", calls: 220, secondsP50: 1.0),
                    ToolBucketRow(tool: "Edit", calls: 160, secondsP50: 1.5),
                ]),
            costUsd: 300.00)
        let second = Session(
            sessionId: "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb", account: "bob@example.com",
            model: "claude-sonnet-5", firstSeenMs: msAgo(2 * 3600), lastSeenMs: msAgo(45),
            requests: 800, inputTokens: 20_000, outputTokens: 90_000,
            cacheReadTokens: 200_000_000,
            tools: SessionTools(
                calls: 1_400, errors: 20, timeouts: 0,
                running: [
                    ToolCall(
                        tool: "Bash", commandHead: "rg -n \"retry\" src/ > hits.log",
                        commandClass: "search", startedMs: msAgo(52))
                ],
                slowest: [
                    ToolCall(
                        tool: "Bash", commandHead: "bun test --coverage", commandClass: "build",
                        endedMs: msAgo(6 * 60), seconds: 60.0)
                ],
                overOneMinute: 15,
                byTool: [
                    ToolBucketRow(tool: "Bash", calls: 1_400, secondsP50: 1.5)
                ]),
            costUsd: 120.00)
        // The 18 the fold exists for: two requests, no tool call, then
        // silence. Idle spread 10 to 50 minutes, matching the live spread, so
        // not one of them is near the five-minute boundary and the scene
        // cannot pass by accident.
        let warmups = (1...18).map { index in
            Session(
                sessionId: String(format: "cccccccc-cccc-cccc-cccc-%012d", index),
                account: index.isMultiple(of: 2) ? "alice@example.com" : "bob@example.com",
                model: "claude-sonnet-5", firstSeenMs: msAgo(3_600 + Double(index) * 120),
                lastSeenMs: msAgo(600 + Double(index) * 140),
                requests: 2, inputTokens: 1_000, outputTokens: 30, cacheReadTokens: 10_000,
                costUsd: 0.10)
        }
        return Fleet(
            accounts: base.accounts, unreadable: base.unreadable,
            sessions: [busy, second] + warmups, sessionsSupported: true)
    }

    /// The three plan labels a real fleet produces, side by side — `Max 20x`,
    /// `Team 5x`, `Team Standard` — plus one row with NO plan at all.
    ///
    /// The fourth row is the point as much as the first three: an account that
    /// has never been profiled must draw no tag, not a guessed one, and a
    /// screenshot is the only place that negative is actually visible.
    private static var planLabelsJSON: String {
        let rows = [
            account(
                "alice@example.com", quota: "0.12", state: "ok",
                plan: "Max 20x", orgUuid: "11111111-1111-1111-1111-111111111111"),
            account(
                "bob@example.com", quota: "0.31", state: "ok",
                plan: "Team 5x", orgUuid: "22222222-2222-2222-2222-222222222222"),
            account(
                "carol@example.com", quota: "0.44", state: "ok",
                plan: "Team Standard", orgUuid: "22222222-2222-2222-2222-222222222222"),
            account("dave@example.com", quota: "0.08", state: "ok"),
        ]
        return "[\(rows.joined(separator: ","))]"
    }

    /// THE WIDEST ROW THE FLEET CAN PRODUCE — the shape that overflowed the
    /// panel, reproduced so a PNG can prove it does not any more.
    ///
    /// Everything that competes for width at once: the control marker, a
    /// reserved group (`GROUP ONLY`), a quota pill, TWO group tags one of which
    /// is long, the longest plan label, and a 30-character email. On the live
    /// fleet this row rendered wider than `Tok.panelWidth`, so both card edges
    /// were clipped and the header truncated to "…eady · 15 ok".
    ///
    /// The pass condition is not "it looks tidy" — it is that NOTHING is
    /// clipped and the name is still readable. A second, ordinary row is
    /// included as the control: if the panel is over-wide for a structural
    /// reason, both rows show it, and the bug is not the one this scene names.
    ///
    /// Carries `groupColors` (`widestRowSceneColors`), which every earlier
    /// version of this fixture omitted. This is also the one scene that puts
    /// ONE account in TWO group sections at once (the group-outline design's
    /// decision #4) — the case the group-outline
    /// feature most needs a real render of, and a fixture with no colours
    /// draws the section-outline feature's neutral FALLBACK stroke in both
    /// sections, which reads as identical grey regardless of whether the two
    /// sections are wired to two different colours or none at all. Without
    /// this, the scene cannot tell "outline colour is broken" apart from
    /// "outline colour was never given one to draw" — the exact ambiguity
    /// that made a rendered PNG of this scene, by itself, prove nothing about
    /// the feature it was chosen to demonstrate.
    private static var widestRowJSON: String {
        let worst = account(
            "henry.fitzgerald@example.com", quota: "0.62", state: "ok",
            groups: ["gil", "henry-team-parked"],
            reservedGroups: ["gil"],
            groupColors: widestRowSceneColors,
            plan: "Team Standard",
            orgUuid: "22222222-2222-2222-2222-222222222222")
        let ordinary = account(
            "bob@example.com", quota: "0.31", state: "ok",
            plan: "Max 20x",
            orgUuid: "11111111-1111-1111-1111-111111111111")
        return "[\(worst),\(ordinary)]"
    }

    /// A PARKED GROUP BESIDE A LIVE ONE — the state a screenshot is the only
    /// honest check on, because every part of it is visual.
    ///
    /// Four rows, and the last two are the point:
    ///  - two members of parked `henry-team`, one of which is ALSO disabled by
    ///    hand. Both draw `PARKED`, but for different reasons, and the fixture
    ///    exists to show that the panel does not need them to look different:
    ///    the consequence is identical, and the group tag says which is which.
    ///  - a member of live `dev`, the control: if the dimming is wrong, or
    ///    applied to every tag, this row shows it.
    ///  - an ungrouped row, so the scene also carries a tag-less baseline.
    ///
    /// The pass condition is that the two parked tags read as held back — dim
    /// wash, pause glyph — while `DEV` beside them stays at full strength and
    /// still identifiable by colour. A tag that dims into illegibility fails
    /// this scene as surely as one that does not dim at all.
    private static var parkedGroupJSON: String {
        let parkedLive = account(
            "alice@example.com", quota: "0.12", state: "ok",
            groups: ["henry-team"], parkedGroups: ["henry-team"],
            groupColors: parkedSceneColors,
            plan: "Max 20x", orgUuid: "11111111-1111-1111-1111-111111111111",
            gate: "parked")
        let parkedAndDisabled = account(
            "bob@example.com", quota: "0.31", state: "ok", disabled: true,
            groups: ["henry-team"], parkedGroups: ["henry-team"],
            groupColors: parkedSceneColors,
            plan: "Team 5x", orgUuid: "22222222-2222-2222-2222-222222222222",
            gate: "disabled")
        let live = account(
            "carol@example.com", quota: "0.44", state: "ok",
            groups: ["dev"], groupColors: parkedSceneColors,
            plan: "Team Standard", orgUuid: "22222222-2222-2222-2222-222222222222",
            gate: "ok")
        let ungrouped = account("dave@example.com", quota: "0.08", state: "ok", gate: "ok")
        return "[\(parkedLive),\(parkedAndDisabled),\(live),\(ungrouped)]"
    }

    /// Real colours for the parked scene: dimming is invisible against the
    /// neutral fallback every other scene draws, so this one carries the
    /// `groupColors` the server actually sends.
    private static let parkedSceneColors: [String: String] = [
        "henry-team": "#32d74b", "dev": "#0a84ff",
    ]

    /// Real colours for the widest-row scene, deliberately DIFFERENT hues
    /// from `parkedSceneColors` (orange, purple, rather than green, blue) —
    /// two scenes both carrying blue/green would leave a swapped-colour bug
    /// invisible if the swap happened to land the same pair back on the same
    /// two sections. `henry.fitzgerald@example.com` sits in BOTH `gil` and
    /// `henry-team-parked` at once, so this is the fixture that shows the
    /// group-outline feature's account-in-two-groups case in colour.
    private static let widestRowSceneColors: [String: String] = [
        "gil": "#ff9f0a", "henry-team-parked": "#bf5af2",
    ]

    /// ONE PERSON, TWO ORGS — the shape that broke the panel, and the names it
    /// wears now that `tcr` has fixed it.
    ///
    /// Both rows used to carry `henry@example.com`; only the org differed. They
    /// collapsed to one SwiftUI identity, so the panel drew the FIRST row's
    /// numbers on both and neither wore its own gate pill, while `tcr status
    /// --json` reported the two correctly and differently. `tcr` now gives the
    /// second row its own name (`src/config.rs`, `migrate_duplicate_names`), so
    /// the PNG must show BOTH halves of the Team row's name and truncate
    /// neither: the email on line one, the `/example-team` half as the first tag
    /// on the designations line. Read left to right across the two lines, that
    /// is exactly what a person types to address the row.
    ///
    /// The email is deliberately a realistic length rather than a short one.
    /// A short address leaves slack that hides the very overflow this scene
    /// exists to make visible — the previous fixture's `henry@example.com`
    /// rendered as `henry@ex…ample-team` the moment the suffix shared its line.
    ///
    /// The two rows are deliberately as unlike each other as a pair can be —
    /// one spent and rejected, one fresh and never probed, and different plans
    /// — because that is what makes the failure legible in a PNG: if the render
    /// shows two identical rows, the collapse is back.
    private static var duplicateEmailJSON: String {
        let old = account(
            "henry.mitchell@example.com", quota: "1.0", state: "spent", probe: "ok", held: hold,
            plan: "Max 20x", orgUuid: "11111111-1111-1111-1111-111111111111",
            gate: "rejected")
        let fresh = account(
            "henry.mitchell@example.com/example-team", quota: "null", state: "ok", probe: "never",
            usage: unmeasuredUsage,
            plan: "Team Standard", orgUuid: "22222222-2222-2222-2222-222222222222",
            gate: "ok")
        return "[\(old),\(fresh)]"
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
        // The near and spent rows carry resets on both windows — what the real
        // fleet looks like, and the only way this scene shows a caption in its
        // window's own colour (`FleetView.captionTint`).
        rows.append(
            account(
                "near@example.com", quota: "0.94", state: "near", held: hold,
                fiveHourResetInMinutes: 47, sevenDayResetInMinutes: 6_498))
        rows += (1...6).map {
            account(
                "spent-\($0)@example.com", quota: "1.0", state: "spent", held: hold,
                fiveHourResetInMinutes: 12 + $0 * 30, sevenDayResetInMinutes: 6_498)
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

    /// `usage` is null here for the same reason the four serving counters are
    /// on a real offline row: there is no serving process to have measured
    /// anything. The spend line and the 5h figures must both be absent.
    private static var offlineJSON: String {
        "[\(account("alice@example.com", quota: "0.12", state: "ok", source: "offline", usage: unmeasuredUsage))]"
    }

    /// Every branch of the spend rendering in one panel, because each one is a
    /// DIFFERENT null and the whole feature is the claim that they never look
    /// alike:
    ///
    ///  - `partial@` — measured, with its own quota window, and 12 of its
    ///    requests unpriceable: `$5.61+ · 12k out`, the `+` saying the figure
    ///    is a floor.
    ///  - `unpriced@` — measured but unpriceable: the token count alone, with
    ///    its unit — `12k out` — and the header's `N unpriced` clause.
    ///  - `no-window@` — priced, but the server cannot name the window's start,
    ///    so the card shows the DAY and says so: `$9.41 today · 32k out`.
    ///  - `unmeasured@` — no `usage` at all: an empty slot, not a zero.
    ///
    /// The header line is the fleet's sum over the three measured rows, so it
    /// also proves the unmeasured one contributes nothing rather than zero. It
    /// names every model: four priced ones, then `sonnet-4-5 ?` — the unpriced
    /// model has real traffic here and must not vanish from the line, and a
    /// percentage is exactly what nobody can compute for it.
    ///
    /// Those five entries are also this scene's LAYOUT case, and the reason it
    /// is worth looking at rather than only asserting on. The line wraps, and a
    /// wrap is charged to the account list (`PanelHeight.headerOverflow`), so
    /// what a reader must check here is that the list still has rows and the
    /// footer has not moved. It read `opus-5 100% · sonnet-4-5 ?` while only
    /// the top two models were named and the rest became `"+2"`.
    ///
    /// The fully-priced card (`$5.61 · 12k out`, no marker) is scene 01's.
    /// The same four rows also carry all four branches of the FABLE weekly
    /// slot on their 7d line, because that slot has the same shape of rule and
    /// the same way of being got wrong:
    ///
    ///  - `partial@` — `near`: `fable 71% · in 4d 12h`, in that window's amber.
    ///  - `unpriced@` — `spent`: `fable 100% · in 4d 12h`, in red. Its 7d
    ///    window is `ok` in the same frame, which is the point: the two are
    ///    independent windows and a reader must be able to see one spent while
    ///    the other has headroom.
    ///  - `no-window@` — a real fraction with NO state word, which is what a
    ///    proxy predating `sevenDayOiState` sends and therefore what the live
    ///    one sends today. Neutral tint, real percentage: nothing borrows the
    ///    composite state to colour it.
    ///  - `unmeasured@` — no fraction at all: an EMPTY slot, never `n/a`.
    private static var usageStatsJSON: String {
        let partial = account(
            "partial@example.com", quota: "0.42", state: "ok",
            fiveHourResetInMinutes: 130, sevenDayResetInMinutes: 4_320,
            sevenDayOi: "0.71", sevenDayOiState: "near", sevenDayOiResetInMinutes: 6_498,
            usage: partiallyPricedUsage)
        let unpriced = account(
            "unpriced@example.com", quota: "0.31", state: "ok",
            sevenDayOi: "1.0", sevenDayOiState: "spent", sevenDayOiResetInMinutes: 6_498,
            usage: unpricedUsage)
        let noWindow = account(
            "no-window@example.com", quota: "0.18", state: "ok",
            sevenDayOi: "0.34", sevenDayOiState: "null",
            usage: noWindowUsage)
        let unmeasured = account(
            "unmeasured@example.com", quota: "0.07", state: "ok",
            sevenDayOi: "null", sevenDayOiState: "null",
            usage: unmeasuredUsage)
        return "[\(partial),\(unpriced),\(noWindow),\(unmeasured)]"
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
