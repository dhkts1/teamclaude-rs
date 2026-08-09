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
    /// scene carries it, which is what makes the new control's ON appearance —
    /// the tinted mark and the caveat line — reviewable, since the real menu bar
    /// cannot be screenshotted here.
    private static var scenes: [(name: String, state: PollState, awake: Bool)] {
        [
            ("01-healthy", .loaded(fleet(healthyJSON)), false),
            ("02-mixed-thirteen", .loaded(fleet(mixedJSON)), false),
            ("03-zero-capacity", .loaded(fleet(exhaustedJSON)), false),
            ("04-unmeasured-row", .loaded(fleet(unmeasuredJSON)), false),
            ("05-unreadable-row", .loaded(partiallyUnreadableFleet()), false),
            ("06-offline-source", .loaded(fleet(offlineJSON)), false),
            ("07-empty-fleet", .loaded(fleet("[]")), false),
            ("08-tool-missing", .toolMissing(searched: ["/usr/local/bin/tcr", "/opt/homebrew/bin/tcr"]), false),
            ("09-command-failed", .commandFailed(exitCode: 1, message: "connection refused"), false),
            ("10-undecodable", .undecodable(message: "DecodingError.valueNotFound: quota"), false),
            ("11-pending", .pending, false),
            ("12-keeping-awake", .loaded(fleet(healthyJSON)), true),
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
        _ scene: (name: String, state: PollState, awake: Bool),
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
                awake: awake,
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
        source: String = "live"
    ) -> String {
        """
        {"name":"\(name)","priority":0,"status":"active","disabled":\(disabled),
         "quota":\(quota),"quotaState":"\(state)","fiveHour":\(quota),
         "sevenDay":\(quota),"sevenDayOi":0.0,"held":\(held),
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
            account("never@example.com", quota: "null", state: "ok",
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
}
