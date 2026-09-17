import Foundation
import XCTest

@testable import TcrBarCore

/// Pins the Tools tab's hard rule: the summary line's "N near timeout" count
/// and the RUNNING NOW row's own ring/tint MUST read one shared expression,
/// ``SessionToolEntry/isNearTimeout(now:timeoutSeconds:warnWithinSeconds:)`` —
/// never two independently-computed booleans that happen to agree today.
///
/// The defect this exists to catch (from the bridge, quoting Gil's own
/// mockup): the summary sentence printed "3 running, none near timeout" over
/// three rows the SAME mockup drew red, because the sentence was hardcoded
/// and the rows were not reading it. A summary that can disagree with the
/// rows beneath it is worse than no summary.
final class ToolsNearTimeoutSharedPredicateTests: XCTestCase {
    private func makeCall(tool: String, secondsAgo: Double, now: Date) -> ToolCall {
        ToolCall(
            tool: tool,
            startedMs: Int64(now.addingTimeInterval(-secondsAgo).timeIntervalSince1970 * 1000))
    }

    func testACappedCallInsideTheWarnBandIsNearTimeout() {
        let now = Date()
        // 600s cap, 590s elapsed -> 10s left, inside a 60s warn band.
        let entry = SessionToolEntry(
            sessionId: "s1", call: makeCall(tool: "Bash", secondsAgo: 590, now: now))
        XCTAssertTrue(
            entry.isNearTimeout(now: now, timeoutSeconds: 600, warnWithinSeconds: 60))
    }

    func testACappedCallWellUnderTheCapIsNotNearTimeout() {
        let now = Date()
        let entry = SessionToolEntry(
            sessionId: "s1", call: makeCall(tool: "Bash", secondsAgo: 10, now: now))
        XCTAssertFalse(
            entry.isNearTimeout(now: now, timeoutSeconds: 600, warnWithinSeconds: 60))
    }

    /// An `Agent` has no cap this build knows about, so however long it has
    /// run it can never be "near" a timeout it does not have — the same rule
    /// ``ToolCall/capped`` states for the ring itself.
    func testAnUncappedCallIsNeverNearATimeoutItDoesNotHave() {
        let now = Date()
        let entry = SessionToolEntry(
            sessionId: "s1", call: makeCall(tool: "Agent", secondsAgo: 20_000, now: now))
        XCTAssertFalse(
            entry.isNearTimeout(now: now, timeoutSeconds: 600, warnWithinSeconds: 6000))
    }

    /// `Fleet.toolsNearTimeoutCount` (what the summary line reads) must equal a
    /// fresh filter of `toolsRunning` (what the row-drawing loop iterates) run
    /// through the exact same predicate — proven by construction, one function
    /// with two callers, rather than by comparing two hand-derived numbers.
    func testFleetCountAgreesWithAFreshFilterOverTheRunningList() {
        let now = Date()
        let session = Session(
            sessionId: "s1",
            firstSeenMs: 0,
            lastSeenMs: 0,
            tools: SessionTools(running: [
                makeCall(tool: "Bash", secondsAgo: 590, now: now),  // near
                makeCall(tool: "Bash", secondsAgo: 10, now: now),  // not near
                makeCall(tool: "Agent", secondsAgo: 590, now: now),  // uncapped, never near
            ]))
        let fleet = Fleet(accounts: [], sessions: [session], sessionsSupported: true)
        let count = fleet.toolsNearTimeoutCount(
            now: now, timeoutSeconds: 600, warnWithinSeconds: 60)
        let filtered = fleet.toolsRunning.filter {
            $0.isNearTimeout(now: now, timeoutSeconds: 600, warnWithinSeconds: 60)
        }.count
        XCTAssertEqual(count, filtered)
        XCTAssertEqual(count, 1)
    }

    /// Pinned at the SOURCE, because the row is a SwiftUI view this bundle
    /// cannot instantiate: `runningToolItem` must compute its ring/tint by
    /// calling `SessionToolEntry.isNearTimeout(...)`, never by re-deriving its
    /// own boolean the way the pre-fix code did
    /// (`let isNearTimeout = (remaining ?? .infinity) <= ...`). If that inline
    /// form comes back, this fails — the row and the summary count can drift
    /// apart again exactly as they did in the mockup.
    func testTheRunningRowReadsTheSharedPredicateNotAHandRolledOne() throws {
        let source = try panelSource("FleetView.swift")
        XCTAssertTrue(
            source.contains("entry.isNearTimeout("),
            "runningToolItem no longer calls SessionToolEntry.isNearTimeout(...) — the row "
                + "and the summary's near-timeout count can silently drift apart again")
        XCTAssertFalse(
            source.contains("let isNearTimeout = (remaining"),
            "the row is back to computing its own near-timeout boolean inline instead of "
                + "reading the shared predicate")
    }

    private func panelSource(_ relative: String) throws -> String {
        let repoRoot = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent()  // -> TcrBarTests
            .deletingLastPathComponent()  // -> Tests
            .deletingLastPathComponent()  // -> apps/macos
            .deletingLastPathComponent()  // -> apps
            .deletingLastPathComponent()  // -> repo root
        let file = repoRoot.appendingPathComponent("apps/macos/Sources/TcrBar/\(relative)")
        return try String(contentsOf: file, encoding: .utf8)
    }
}
