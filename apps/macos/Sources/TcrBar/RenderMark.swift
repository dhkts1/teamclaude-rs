import AppKit
import TcrBarCore

/// Rasterise the menu-bar mark itself — the composed `NSImage`
/// ``MenuBarMark/image(gaugeSymbol:awake:awakeTint:)`` builds, plus the
/// `ready/enabled` label this feature (F5, `data/plans/menubar-counts-bridge.md`)
/// adds beside it — to PNG, in-process, then exit.
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
///     TcrBar.app/Contents/MacOS/TcrBar --render-mark /tmp/tcrbar-mark
///
/// Writes one PNG per scene and exits without ever showing a menu-bar item,
/// polling `tcr`, or touching a server.
enum RenderMark {
    static let flag = "--render-mark"

    static func requestedDirectory(_ arguments: [String] = CommandLine.arguments) -> URL? {
        guard let i = arguments.firstIndex(of: flag), i + 1 < arguments.count else { return nil }
        return URL(fileURLWithPath: arguments[i + 1])
    }

    /// A handful of states worth looking at: a mixed fleet (label visible), an
    /// empty fleet and a failed poll (label hidden — the exact two conditions
    /// ``PollState/countsLabel`` documents), and the mixed fleet again with
    /// `showCounts` off, so the "hidden by preference" branch is also on
    /// disk to look at rather than only unit-tested.
    private static var scenes: [(name: String, state: PollState, showCounts: Bool)] {
        [
            ("01-mixed-fleet-counts-on", .loaded(mixedFleet), true),
            ("02-mixed-fleet-counts-off", .loaded(mixedFleet), false),
            ("03-empty-fleet", .loaded(Fleet(accounts: [])), true),
            (
                "04-poll-failed", .commandFailed(exitCode: 1, message: "connection refused"), true
            ),
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
    /// text needs, at 2x — wide enough that a two-digit `"12/13"` is never
    /// clipped, generous rather than measured, since this is a review artifact
    /// and not a layout contract.
    private static let canvasSize = NSSize(width: 160, height: 44)

    @MainActor
    private static func render(
        _ scene: (name: String, state: PollState, showCounts: Bool),
        into directory: URL
    ) -> Bool {
        let gauge = MenuBarShell.gaugeSymbol(for: scene.state)
        guard let mark = MenuBarMark.image(gaugeSymbol: gauge, awake: false, awakeTint: .systemCyan)
        else {
            FileHandle.standardError.write(Data("no such SF Symbol: \(gauge)\n".utf8))
            return false
        }

        let label = scene.showCounts ? scene.state.countsLabel : nil
        let font = NSFont.monospacedDigitSystemFont(
            ofSize: NSFont.menuBarFont(ofSize: 0).pointSize, weight: .regular)

        let composed = NSImage(size: canvasSize, flipped: false) { rect in
            NSColor.windowBackgroundColor.setFill()
            rect.fill()
            let markRect = NSRect(
                x: 8, y: (rect.height - mark.size.height) / 2,
                width: mark.size.width, height: mark.size.height)
            mark.draw(in: markRect, from: .zero, operation: .sourceOver, fraction: 1)
            if let label {
                let attributed = NSAttributedString(
                    string: label,
                    attributes: [.font: font, .foregroundColor: NSColor.labelColor])
                let labelOrigin = NSPoint(
                    x: markRect.maxX + 4, y: (rect.height - attributed.size().height) / 2)
                attributed.draw(at: labelOrigin)
            }
            return true
        }

        let name = "\(scene.name).png"
        guard let tiff = composed.tiffRepresentation,
            let rep = NSBitmapImageRep(data: tiff),
            let png = rep.representation(using: .png, properties: [:])
        else {
            FileHandle.standardError.write(Data("render failed: \(name)\n".utf8))
            return false
        }

        let url = directory.appendingPathComponent(name)
        do {
            try png.write(to: url)
            print("  \(name)  label=\(label ?? "(hidden)")")
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
}
