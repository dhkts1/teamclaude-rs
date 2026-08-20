import XCTest

@testable import TcrBarCore

/// `UpdateState` — the enum ``Updater`` (in the `TcrBar` executable target)
/// populates from Sparkle's real delegate callbacks. This file tests the
/// state's own semantics: what each case means and what the header shows
/// for it (bridge: `docs/plans/menu-and-update-bridge.md`, "Nothing shows
/// whether an update is available").
///
/// It cannot exercise `Updater`'s Sparkle wiring itself: `Package.swift`
/// deliberately keeps `TcrBarCore` (and therefore this test target, which
/// only depends on it) free of Sparkle, so `SPUUpdater`/`SUAppcastItem`
/// are not constructible here. That boundary is exactly why the mapping
/// logic lives on this framework-free enum rather than inline in the
/// delegate methods — this is the part of "did the callback map to the
/// right case" that a test in this target can actually reach.
final class UpdateStateTests: XCTestCase {

    /// `.unknown` is the honest starting value — before any check has
    /// completed the app does not know, and it must not render as though it
    /// does.
    func testUnknownRendersNothing() {
        XCTAssertNil(UpdateState.unknown.headerMessage)
    }

    /// `.upToDate` also renders nothing — no permanent "you're up to date"
    /// row in a 380pt panel.
    func testUpToDateRendersNothing() {
        XCTAssertNil(UpdateState.upToDate.headerMessage)
    }

    /// `.available` is the one state actually worth a line, and it carries
    /// the version Sparkle reported.
    func testAvailableRendersTheVersion() {
        XCTAssertEqual(
            UpdateState.available(version: "1.2.3").headerMessage,
            "Update available: 1.2.3"
        )
    }

    /// `.failed` also renders — a silently broken feed must stay visible —
    /// carrying Sparkle's own error text verbatim.
    func testFailedRendersTheReason() {
        XCTAssertEqual(
            UpdateState.failed("feed unreachable").headerMessage,
            "Update check failed: feed unreachable"
        )
    }

    /// The four cases are pairwise distinct so a state transition is never
    /// silently absorbed into another case.
    func testCasesAreDistinct() {
        let states: [UpdateState] = [
            .unknown, .upToDate, .available(version: "1.0"), .failed("x"),
        ]
        for (i, a) in states.enumerated() {
            for (j, b) in states.enumerated() where i != j {
                XCTAssertNotEqual(a, b, "\(a) should not equal \(b)")
            }
        }
    }
}
