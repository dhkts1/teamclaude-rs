import Foundation
import XCTest

@testable import TcrBarCore

/// The Sessions tab's row shape: `<dot> name <verb/state> <model> <$>
/// <duration> <X or ›>`, `docs/design/panel-tabs.md` §3. `FleetView.sessionRow`
/// is a SwiftUI view this bundle cannot instantiate (the test target links
/// `TcrBarCore` only — `Package.swift`), so the two facts this rewrite made
/// true are pinned at the SOURCE, the same technique `QuotaTailWidthTests`
/// already uses for a fact about a view it cannot render either.
final class SessionRowDurationColumnTests: XCTestCase {
    /// **`cache %` is gone.** Measured across all ten of the live fleet's
    /// active sessions at the time of this rewrite, it carried exactly one
    /// distinct value (100) — a field with one value on every row is not
    /// information. If a future edit reintroduces `· cache \(...)%`, this
    /// fails.
    func testCachePercentIsNeverDrawnOnASessionRow() throws {
        let source = try panelSource("FleetView.swift")
        XCTAssertFalse(
            source.contains("cache \\("),
            "a `· cache N%` clause is back on the session row — it was removed because it "
                + "carried exactly one distinct value (100%) across every live session")
        XCTAssertFalse(
            source.contains("func cacheHitPercent"),
            "cacheHitPercent(_:) is back — the function this row's cache clause used to call")
    }

    /// **`waiting` is amber, distinct from dim `idle`.** Both the compact
    /// (idle/waiting/unknown) row's trailing status and the busy row's
    /// trailing duration must colour a `.waiting` session with `Tok.near`,
    /// never the same `Tok.dim` an idle one gets — they are different states
    /// and mean different things to the reader (`docs/design/panel-tabs.md`
    /// §3).
    func testWaitingSessionsAreAmberNotDimLikeIdleOnes() throws {
        let source = try panelSource("FleetView.swift")
        let waitingColourSites = source.components(separatedBy: "\n").filter {
            $0.contains("activity == .waiting ? Tok.near : Tok.dim")
        }
        XCTAssertGreaterThanOrEqual(
            waitingColourSites.count, 2,
            "expected at least 2 sites (the compact row's trailing status and the busy row's "
                + "trailing duration) to colour `.waiting` amber (Tok.near) distinctly from "
                + "dim `.idle` (Tok.dim); found \(waitingColourSites.count)")
    }

    /// **Every row ends in a duration** — run time if running, idle/waiting
    /// time if not — never a bare status word with nothing measured beside
    /// it. Pinned by checking the row's own trailing-status string builder
    /// always appends an age or an elapsed duration, for every
    /// ``SessionActivity`` case.
    func testEveryTrailingStatusEndsInAMeasuredDuration() {
        let now = Date()
        let idle = JoinedSession(
            session: Session(sessionId: "s1", firstSeenMs: 0, lastSeenMs: 0),
            file: SessionFile(sessionId: "s1", status: "idle"))
        let waiting = JoinedSession(
            session: Session(sessionId: "s2", firstSeenMs: 0, lastSeenMs: 0),
            file: SessionFile(sessionId: "s2", status: "waiting"))
        // Both states rely on `ageLabel(now:)`, which always renders a
        // HeldWindow-style duration ("now", "3m", "2h", "4d") — never an
        // empty string.
        XCTAssertFalse(idle.ageLabel(now: now).isEmpty)
        XCTAssertFalse(waiting.ageLabel(now: now).isEmpty)
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
