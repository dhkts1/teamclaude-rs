import AppKit
import TcrBarCore

/// Rasterise the menu-bar mark itself — the composed `NSImage`
/// ``MenuBarMark/image(fraction:tint:)`` builds, plus the opt-in
/// `ready/enabled` label (F5) beside it
/// when a scene turns it on — to PNG, in-process, then exit.
///
/// ## Why this exists, separate from `--render-states`
///
/// `RenderStates` draws `FleetView` with `ImageRenderer`, which never touches the
/// real `NSStatusItem` — the mark is composed straight onto `button.image`/
/// `button.attributedTitle` (`MenuBarShell.updateMark`), a path `ImageRenderer`
/// cannot reach because it is not a SwiftUI view at all. `MenuBarMark.image` IS
/// rasterisable headless, though: it is a plain `NSImage(size:flipped:drawingHandler:)`,
/// the same shape `AppIcon.image(size:)` already rasterises for `--render-icon`. So
/// this composes gauge + label the same way `updateMark` does, onto a canvas sized
/// like a real `NSStatusBarButton`, instead of adding a `--render-mark` scene to
/// `RenderStates` — a file this feature's own bridge keeps out of, matching the
/// concurrent lane already working there.
///
/// ## Usage
///
///     TcrBar.app/Contents/MacOS/TcrBar --render-mark <output-directory>
///
/// Writes one PNG per scene and exits without ever showing a menu-bar item,
/// polling `tcr`, or touching a server.
enum RenderMark {
    static let flag = "--render-mark"

    static func requestedDirectory(_ arguments: [String] = CommandLine.arguments) -> URL? {
        guard let i = arguments.firstIndex(of: flag), i + 1 < arguments.count else { return nil }
        return URL(fileURLWithPath: arguments[i + 1])
    }

    /// The coffee-cup mark's own state set (`docs/design/menubar-mark-mockup.html`
    /// § "Coffee-mark rule"), one scene per named point in the acceptance gate —
    /// "0%, 40%, 100%, near, failed, off" — each carrying keep-awake ON except
    /// the one scene named `off`, plus a light-appearance repeat of the 100%
    /// scene the same way the predecessor mockup repeated one state under the
    /// other appearance rather than a different fleet. `appearance` is `nil`
    /// for the process default (dark) and `.aqua` only for the light scene.
    /// `knocks` is how many Macs are waiting on an answer. Its own scenes
    /// rather than a flag on an existing one: the segment is drawn whether or
    /// not the counts label is on, so it takes a scene with counts OFF (the
    /// default Mac) and one with them on beside a second asking Mac, and the
    /// glyph choice at 13 pt cannot be judged in prose.
    private static var scenes:
        [
            (
                name: String, state: PollState, awake: Bool, showCounts: Bool,
                showRunningTools: Bool, appearance: NSAppearance.Name?, knocks: Int
            )
        ]
    {
        [
            ("01-dark-awake-0pct", .loaded(noneReadyFleet), true, false, false, nil, 0),
            ("02-dark-awake-40pct", .loaded(partialReadyFleet), true, false, false, nil, 0),
            ("03-dark-awake-100pct", .loaded(fullReadyFleet), true, false, false, nil, 0),
            ("04-dark-near", .loaded(nearTheLimitFleet), true, false, false, nil, 0),
            (
                "05-dark-failed", .commandFailed(exitCode: 1, message: "connection refused"),
                true, false, false, nil, 0
            ),
            ("06-dark-off-template", .loaded(partialReadyFleet), false, false, false, nil, 0),
            ("07-dark-counts-on", .loaded(mixedFleet), true, true, false, nil, 0),
            ("08-dark-running-tools-count", .loaded(runningToolsFleet), true, true, true, nil, 0),
            ("09-light-awake-100pct", .loaded(fullReadyFleet), true, false, false, .aqua, 0),
            // One Mac asking, counts OFF, which is the default Mac: the glyph
            // and the cup, nothing else.
            ("10-dark-knock-one", .loaded(partialReadyFleet), true, false, false, nil, 1),
            // Two asking, counts and running tools on: the fullest the item
            // ever gets, and the order it is fixed in — what wants an answer,
            // then what the fleet is doing.
            ("11-dark-knock-two-counts-on", .loaded(runningToolsFleet), true, true, true, nil, 2),
            ("12-light-knock-one", .loaded(partialReadyFleet), true, false, false, .aqua, 1),
            ("13-light-knock-two-counts-on", .loaded(mixedFleet), true, true, false, .aqua, 2),
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
        for scene in scenes {
            if render(scene, into: directory) { written += 1 }
        }
        print("\nrendered \(written)/\(scenes.count) images into \(directory.path)")
        exit(written == scenes.count ? 0 : 1)
    }

    /// The width `NSFont.menuBarFont(ofSize: 0)`'s tallest glyph plus the label
    /// text needs, at 2x — wide enough that a two-digit `"12/13"` plus the
    /// running-tools segment is never clipped, generous rather than measured,
    /// since this is a review artifact and not a layout contract.
    private static let canvasSize = NSSize(width: 220, height: 44)

    @MainActor
    private static func render(
        _ scene: (
            name: String, state: PollState, awake: Bool, showCounts: Bool,
            showRunningTools: Bool, appearance: NSAppearance.Name?, knocks: Int
        ),
        into directory: URL
    ) -> Bool {
        let tint = MenuBarShell.cupTint(for: scene.state, awake: scene.awake)
        guard
            let mark = MenuBarMark.image(
                fraction: scene.state.capacityFraction, tint: tint, knocks: scene.knocks)
        else {
            FileHandle.standardError.write(Data("no such SF Symbol: \(MenuBarMark.symbolName)\n".utf8))
            return false
        }

        let label = scene.showCounts ? scene.state.countsLabel : nil
        let running = scene.state.runningToolsCount(showRunningTools: scene.showRunningTools)
        // The TITLE the live item draws, through the same builder
        // `updateMark` calls, so the knock segment cannot be drawn one way in
        // a fixture and another way on the bar. The knock glyph is LEFT of the
        // cup on the real status item, which composes image then title; this
        // canvas draws the cup first for the same reason it always has, so
        // read the two as one item rather than as a pixel-exact placement.
        let title = MenuBarShell.markTitle(
            state: scene.state, showCounts: scene.showCounts,
            showRunningTools: scene.showRunningTools, knocks: scene.knocks)

        let draw: (NSRect) -> Bool = { rect in
            NSColor.windowBackgroundColor.setFill()
            rect.fill()
            let markRect = NSRect(
                x: 8, y: (rect.height - mark.size.height) / 2,
                width: mark.size.width, height: mark.size.height)
            mark.draw(in: markRect, from: .zero, operation: .sourceOver, fraction: 1)
            if title.length > 0 {
                let titleOrigin = NSPoint(
                    x: markRect.maxX + 4, y: (rect.height - title.size().height) / 2)
                title.draw(at: titleOrigin)
            }
            return true
        }

        // Rasterised (`tiffRepresentation`, which is what actually invokes
        // `draw`) INSIDE `performAsCurrentDrawingAppearance` where an
        // appearance override applies, same as `MenuBarMarkTests.rasterise`.
        // `NSImage(size:flipped:drawingHandler:)`'s handler is lazy — it does
        // not run at construction, so building the image inside the block and
        // rasterising it outside would resolve every dynamic colour
        // (`NSColor.labelColor`, `Tok.nearNSColor`, `.windowBackgroundColor`)
        // against whatever appearance is current by the time something
        // outside this function first asks for pixels, not the one this scene
        // asked for.
        func rasterise() -> Data? {
            NSImage(size: canvasSize, flipped: false, drawingHandler: draw).tiffRepresentation
        }
        let tiffData: Data?
        if let appearanceName = scene.appearance, let appearance = NSAppearance(named: appearanceName)
        {
            var result: Data?
            appearance.performAsCurrentDrawingAppearance {
                result = rasterise()
            }
            tiffData = result
        } else {
            tiffData = rasterise()
        }

        let name = "\(scene.name).png"
        guard let tiff = tiffData,
            let rep = NSBitmapImageRep(data: tiff),
            let png = rep.representation(using: .png, properties: [:])
        else {
            FileHandle.standardError.write(Data("render failed: \(name)\n".utf8))
            return false
        }

        let url = directory.appendingPathComponent(name)
        do {
            try png.write(to: url)
            let runningNote = running.map { " running=\($0)" } ?? ""
            print(
                "  \(name)  label=\(label ?? "(hidden)")\(runningNote) knocks=\(scene.knocks)")
            return true
        } catch {
            FileHandle.standardError.write(Data("write failed \(name): \(error)\n".utf8))
            return false
        }
    }

    /// Obviously-fake names only — this repository is public.
    private static let mixedFleetJSON = """
        [
          {
            "source": "live", "serverSha": "abc1234", "serverDirty": false,
            "name": "alice@example.com", "priority": 1, "status": "active",
            "disabled": false, "quota": 0.1, "quotaState": "ok",
            "fiveHour": 0.1, "sevenDay": 0.1, "sevenDayOi": 0.0,
            "held": [], "requests": 10, "inputTokens": 100, "outputTokens": 10,
            "cacheReadTokens": 0, "cacheHitRatio": 0.5, "probeStatus": "ok",
            "probeError": null, "lastStreamError": null, "streamErrorCount": 0
          },
          {
            "source": "live", "serverSha": "abc1234", "serverDirty": false,
            "name": "bob@example.com", "priority": 2, "status": "active",
            "disabled": false, "quota": 0.2, "quotaState": "ok",
            "fiveHour": 0.2, "sevenDay": 0.2, "sevenDayOi": 0.0,
            "held": [], "requests": 20, "inputTokens": 200, "outputTokens": 20,
            "cacheReadTokens": 0, "cacheHitRatio": 0.5, "probeStatus": "ok",
            "probeError": null, "lastStreamError": null, "streamErrorCount": 0
          },
          {
            "source": "live", "serverSha": "abc1234", "serverDirty": false,
            "name": "carol@example.com", "priority": 3, "status": "held",
            "disabled": false, "quota": 0.85, "quotaState": "near",
            "fiveHour": 0.3, "sevenDay": 0.85, "sevenDayOi": 0.1,
            "held": [{"window": "5h", "minutesUntilReset": 93, "resetAtMs": 999000000000}],
            "requests": 5, "inputTokens": 50, "outputTokens": 5,
            "cacheReadTokens": 0, "cacheHitRatio": 0.4, "probeStatus": "ok",
            "probeError": null, "lastStreamError": null, "streamErrorCount": 0
          },
          {
            "source": "live", "serverSha": "abc1234", "serverDirty": false,
            "name": "dave@example.com", "priority": 4, "status": "active",
            "disabled": false, "quota": null, "quotaState": "ok",
            "fiveHour": null, "sevenDay": null, "sevenDayOi": null,
            "held": [], "requests": 0, "inputTokens": 0, "outputTokens": 0,
            "cacheReadTokens": 0, "cacheHitRatio": null, "probeStatus": "never",
            "probeError": null, "lastStreamError": null, "streamErrorCount": 0
          },
          {
            "source": "live", "serverSha": "abc1234", "serverDirty": false,
            "name": "erin@example.com", "priority": 5, "status": "active",
            "disabled": true, "quota": 0.1, "quotaState": "ok",
            "fiveHour": 0.1, "sevenDay": 0.1, "sevenDayOi": 0.0,
            "held": [], "requests": 0, "inputTokens": 0, "outputTokens": 0,
            "cacheReadTokens": 0, "cacheHitRatio": null, "probeStatus": "ok",
            "probeError": null, "lastStreamError": null, "streamErrorCount": 0
          }
        ]
        """

    private static var mixedFleet: Fleet {
        (try? Fleet.decode(Data(mixedFleetJSON.utf8))) ?? Fleet(accounts: [])
    }

    /// Two enabled accounts, neither ready and neither near — `0/2`, the cup's
    /// empty scene. Distinct from the near-the-limit fixture below: this one
    /// must NOT trip `capacityGlyphState == .near`, so the cup draws its
    /// default tint (template/awake) at fraction 0, not amber.
    private static let noneReadyJSON = """
        [
          {
            "source": "live", "serverSha": "abc1234", "serverDirty": false,
            "name": "henry@example.com", "priority": 1, "status": "active",
            "disabled": false, "quota": 1.0, "quotaState": "spent",
            "fiveHour": 1.0, "sevenDay": 1.0, "sevenDayOi": 0.0,
            "held": [{"window": "5h", "minutesUntilReset": 200, "resetAtMs": 999000000000}],
            "requests": 9, "inputTokens": 90, "outputTokens": 9,
            "cacheReadTokens": 0, "cacheHitRatio": 0.5, "probeStatus": "ok",
            "probeError": null, "lastStreamError": null, "streamErrorCount": 0
          },
          {
            "source": "live", "serverSha": "abc1234", "serverDirty": false,
            "name": "iris@example.com", "priority": 2, "status": "active",
            "disabled": false, "quota": 1.0, "quotaState": "spent",
            "fiveHour": 1.0, "sevenDay": 1.0, "sevenDayOi": 0.0,
            "held": [{"window": "5h", "minutesUntilReset": 150, "resetAtMs": 999000000000}],
            "requests": 4, "inputTokens": 40, "outputTokens": 4,
            "cacheReadTokens": 0, "cacheHitRatio": 0.5, "probeStatus": "ok",
            "probeError": null, "lastStreamError": null, "streamErrorCount": 0
          }
        ]
        """

    private static var noneReadyFleet: Fleet {
        (try? Fleet.decode(Data(noneReadyJSON.utf8))) ?? Fleet(accounts: [])
    }

    /// Five enabled accounts, two ready — `2/5`, `0.4`, the cup's 40% scene.
    private static let partialReadyJSON = """
        [
          {
            "source": "live", "serverSha": "abc1234", "serverDirty": false,
            "name": "jack@example.com", "priority": 1, "status": "active",
            "disabled": false, "quota": 0.1, "quotaState": "ok",
            "fiveHour": 0.1, "sevenDay": 0.1, "sevenDayOi": 0.0,
            "held": [], "requests": 6, "inputTokens": 60, "outputTokens": 6,
            "cacheReadTokens": 0, "cacheHitRatio": 0.5, "probeStatus": "ok",
            "probeError": null, "lastStreamError": null, "streamErrorCount": 0
          },
          {
            "source": "live", "serverSha": "abc1234", "serverDirty": false,
            "name": "kim@example.com", "priority": 2, "status": "active",
            "disabled": false, "quota": 0.2, "quotaState": "ok",
            "fiveHour": 0.2, "sevenDay": 0.2, "sevenDayOi": 0.0,
            "held": [], "requests": 3, "inputTokens": 30, "outputTokens": 3,
            "cacheReadTokens": 0, "cacheHitRatio": 0.5, "probeStatus": "ok",
            "probeError": null, "lastStreamError": null, "streamErrorCount": 0
          },
          {
            "source": "live", "serverSha": "abc1234", "serverDirty": false,
            "name": "liam@example.com", "priority": 3, "status": "active",
            "disabled": false, "quota": 1.0, "quotaState": "spent",
            "fiveHour": 1.0, "sevenDay": 1.0, "sevenDayOi": 0.0,
            "held": [{"window": "5h", "minutesUntilReset": 100, "resetAtMs": 999000000000}],
            "requests": 2, "inputTokens": 20, "outputTokens": 2,
            "cacheReadTokens": 0, "cacheHitRatio": 0.5, "probeStatus": "ok",
            "probeError": null, "lastStreamError": null, "streamErrorCount": 0
          },
          {
            "source": "live", "serverSha": "abc1234", "serverDirty": false,
            "name": "maya@example.com", "priority": 4, "status": "active",
            "disabled": false, "quota": 1.0, "quotaState": "spent",
            "fiveHour": 1.0, "sevenDay": 1.0, "sevenDayOi": 0.0,
            "held": [{"window": "5h", "minutesUntilReset": 110, "resetAtMs": 999000000000}],
            "requests": 1, "inputTokens": 10, "outputTokens": 1,
            "cacheReadTokens": 0, "cacheHitRatio": 0.5, "probeStatus": "ok",
            "probeError": null, "lastStreamError": null, "streamErrorCount": 0
          },
          {
            "source": "live", "serverSha": "abc1234", "serverDirty": false,
            "name": "noah@example.com", "priority": 5, "status": "active",
            "disabled": false, "quota": 1.0, "quotaState": "spent",
            "fiveHour": 1.0, "sevenDay": 1.0, "sevenDayOi": 0.0,
            "held": [{"window": "5h", "minutesUntilReset": 120, "resetAtMs": 999000000000}],
            "requests": 1, "inputTokens": 10, "outputTokens": 1,
            "cacheReadTokens": 0, "cacheHitRatio": 0.5, "probeStatus": "ok",
            "probeError": null, "lastStreamError": null, "streamErrorCount": 0
          }
        ]
        """

    private static var partialReadyFleet: Fleet {
        (try? Fleet.decode(Data(partialReadyJSON.utf8))) ?? Fleet(accounts: [])
    }

    /// Two enabled accounts, both ready — `2/2`, `1.0`, the cup's full scene.
    private static let fullReadyJSON = """
        [
          {
            "source": "live", "serverSha": "abc1234", "serverDirty": false,
            "name": "olive@example.com", "priority": 1, "status": "active",
            "disabled": false, "quota": 0.05, "quotaState": "ok",
            "fiveHour": 0.05, "sevenDay": 0.05, "sevenDayOi": 0.0,
            "held": [], "requests": 2, "inputTokens": 20, "outputTokens": 2,
            "cacheReadTokens": 0, "cacheHitRatio": 0.5, "probeStatus": "ok",
            "probeError": null, "lastStreamError": null, "streamErrorCount": 0
          },
          {
            "source": "live", "serverSha": "abc1234", "serverDirty": false,
            "name": "pat@example.com", "priority": 2, "status": "active",
            "disabled": false, "quota": 0.05, "quotaState": "ok",
            "fiveHour": 0.05, "sevenDay": 0.05, "sevenDayOi": 0.0,
            "held": [], "requests": 1, "inputTokens": 10, "outputTokens": 1,
            "cacheReadTokens": 0, "cacheHitRatio": 0.5, "probeStatus": "ok",
            "probeError": null, "lastStreamError": null, "streamErrorCount": 0
          }
        ]
        """

    private static var fullReadyFleet: Fleet {
        (try? Fleet.decode(Data(fullReadyJSON.utf8))) ?? Fleet(accounts: [])
    }

    /// Zero ready, at least one near — the exact condition
    /// ``Fleet/capacityGlyphState``'s `.near` case tests, so this scene
    /// exercises the amber count for real rather than asserting the rule and
    /// drawing a fleet that happens not to trigger it. Neither account carries
    /// `quotaState: "ok"`, which is what keeps ``Account/isReady`` false for
    /// both.
    private static let nearTheLimitJSON = """
        [
          {
            "source": "live", "serverSha": "abc1234", "serverDirty": false,
            "name": "frank@example.com", "priority": 1, "status": "held",
            "disabled": false, "quota": 0.9, "quotaState": "near",
            "fiveHour": 0.4, "sevenDay": 0.9, "sevenDayOi": 0.05,
            "held": [{"window": "5h", "minutesUntilReset": 40, "resetAtMs": 999000000000}],
            "requests": 8, "inputTokens": 80, "outputTokens": 8,
            "cacheReadTokens": 0, "cacheHitRatio": 0.5, "probeStatus": "ok",
            "probeError": null, "lastStreamError": null, "streamErrorCount": 0
          },
          {
            "source": "live", "serverSha": "abc1234", "serverDirty": false,
            "name": "grace@example.com", "priority": 2, "status": "held",
            "disabled": false, "quota": 1.0, "quotaState": "spent",
            "fiveHour": 1.0, "sevenDay": 1.0, "sevenDayOi": 0.0,
            "held": [{"window": "5h", "minutesUntilReset": 180, "resetAtMs": 999000000000}],
            "requests": 3, "inputTokens": 30, "outputTokens": 3,
            "cacheReadTokens": 0, "cacheHitRatio": 0.3, "probeStatus": "ok",
            "probeError": null, "lastStreamError": null, "streamErrorCount": 0
          }
        ]
        """

    private static var nearTheLimitFleet: Fleet {
        (try? Fleet.decode(Data(nearTheLimitJSON.utf8))) ?? Fleet(accounts: [])
    }

    /// The mixed fleet plus three live sessions with a running Bash call each
    /// — matching the mockup's "3 tools running" example. `sessionsSupported:
    /// true` is load-bearing: it is what ``PollState/runningToolsCount(showRunningTools:)``
    /// checks before drawing the segment, exactly the field
    /// ``Fleet/decode(_:)`` never sets (see that method's own doc-comment), so
    /// this fixture is built directly rather than decoded, the same way
    /// `RenderStates.sessionsTabFleet` already does.
    private static var runningToolsFleet: Fleet {
        let base = mixedFleet
        func msAgo(_ seconds: TimeInterval) -> Int64 {
            Int64(Date().addingTimeInterval(-seconds).timeIntervalSince1970 * 1000)
        }
        let sessions = [
            Session(
                sessionId: "aaaaaaaa-0001", account: "alice@example.com",
                firstSeenMs: msAgo(3600), lastSeenMs: msAgo(30),
                tools: SessionTools(
                    calls: 12, running: [
                        ToolCall(tool: "Bash", commandHead: "cargo test", startedMs: msAgo(20))
                    ])),
            Session(
                sessionId: "bbbbbbbb-0002", account: "bob@example.com",
                firstSeenMs: msAgo(1800), lastSeenMs: msAgo(15),
                tools: SessionTools(
                    calls: 4, running: [
                        ToolCall(tool: "Bash", commandHead: "swift build", startedMs: msAgo(10))
                    ])),
            Session(
                sessionId: "cccccccc-0003", account: "bob@example.com",
                firstSeenMs: msAgo(900), lastSeenMs: msAgo(5),
                tools: SessionTools(
                    calls: 2, running: [
                        ToolCall(tool: "Read", commandHead: nil, startedMs: msAgo(3))
                    ])),
        ]
        return Fleet(
            accounts: base.accounts, unreadable: base.unreadable, sessions: sessions,
            sessionsSupported: true)
    }
}
