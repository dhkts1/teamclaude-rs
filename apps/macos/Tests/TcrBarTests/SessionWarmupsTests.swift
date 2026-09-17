import XCTest

@testable import TcrBarCore

/// Half the Sessions tab was cache-warm stubs — two requests, no tool call,
/// then silence. Measured live 2026-09-17: 28 sessions, 14 matching all three
/// clauses, together $2.06.
final class SessionWarmupsTests: XCTestCase {
    private let now = Date(timeIntervalSince1970: 1_000_000)

    /// `lastSeenMs` `idleMinutes` before ``now``.
    private func session(
        id: String, requests: Int, calls: Int, idleMinutes: Double, costUsd: Double? = nil
    ) -> Session {
        let lastSeenMs = Int64((now.timeIntervalSince1970 - idleMinutes * 60) * 1000)
        return Session(
            sessionId: id, firstSeenMs: lastSeenMs, lastSeenMs: lastSeenMs, requests: requests,
            tools: SessionTools(calls: calls), costUsd: costUsd)
    }

    private func joined(_ session: Session) -> JoinedSession {
        JoinedSession(session: session, file: nil)
    }

    // MARK: - isWarmup

    /// All three clauses present: a warm-up.
    func testAllThreeClausesMakeAWarmup() {
        let s = session(id: "s1", requests: 2, calls: 0, idleMinutes: 10)
        XCTAssertTrue(SessionWarmup.isWarmup(joined(s), now: now))
    }

    /// Over the request cap: not a warm-up, regardless of idle time.
    func testOverTheRequestCapIsNotAWarmup() {
        let s = session(id: "s1", requests: 3, calls: 0, idleMinutes: 10)
        XCTAssertFalse(SessionWarmup.isWarmup(joined(s), now: now))
    }

    /// A tool call ran: not a warm-up, even with only one request.
    func testAToolCallIsNotAWarmup() {
        let s = session(id: "s1", requests: 1, calls: 1, idleMinutes: 10)
        XCTAssertFalse(SessionWarmup.isWarmup(joined(s), now: now))
    }

    /// WITHOUT the idle clause a session that just started (0 requests, 0
    /// calls, seen 1 minute ago) would be folded — exactly the session an
    /// operator most wants to see appear. The idle clause keeps it visible.
    func testASessionSeenOneMinuteAgoIsNotFolded() {
        let s = session(id: "s1", requests: 1, calls: 0, idleMinutes: 1)
        XCTAssertFalse(SessionWarmup.isWarmup(joined(s), now: now))
    }

    /// Exactly on the 5-minute boundary is still fresh, per the `>` (not
    /// `>=`) in ``SessionWarmup/idleThresholdSeconds``.
    func testExactlyFiveMinutesIdleIsNotYetAWarmup() {
        let s = session(id: "s1", requests: 1, calls: 0, idleMinutes: 5)
        XCTAssertFalse(SessionWarmup.isWarmup(joined(s), now: now))
    }

    // MARK: - SessionsFold

    /// The measured shape: 3 real sessions (one busy with a tool call, one
    /// fresh, one over the request cap) plus 14 warm-ups.
    func testFoldSplitsRealSessionsFromWarmups() {
        var sessions: [JoinedSession] = [
            joined(session(id: "busy", requests: 40, calls: 12, idleMinutes: 0)),
            joined(session(id: "fresh", requests: 1, calls: 0, idleMinutes: 1)),
            joined(session(id: "chatty", requests: 5, calls: 0, idleMinutes: 10)),
        ]
        for i in 0..<14 {
            sessions.append(
                joined(
                    session(
                        id: "warmup-\(i)", requests: 2, calls: 0, idleMinutes: 10, costUsd: 0.147)))
        }
        let fold = SessionsFold(sessions, now: now)
        XCTAssertEqual(fold.visible.count, 3)
        XCTAssertEqual(fold.warmups.count, 14)
        // Rounds to the measured $2.06 (14 * 0.147 = 2.058).
        XCTAssertEqual(fold.warmupCost.map { ($0 * 100).rounded() / 100 }, 2.06)
        XCTAssertEqual(fold.foldLabel, "14 warm-ups · $2.06")
    }

    /// The header count is the WHOLE fleet, not just what's still drawn as a
    /// row — folding fourteen sessions into a summary line is a display
    /// choice, not a claim that fourteen sessions stopped existing.
    func testTotalCountIncludesFoldedWarmups() {
        var sessions: [JoinedSession] = [joined(session(id: "real", requests: 5, calls: 1, idleMinutes: 0))]
        for i in 0..<14 {
            sessions.append(joined(session(id: "warmup-\(i)", requests: 2, calls: 0, idleMinutes: 10)))
        }
        let fold = SessionsFold(sessions, now: now)
        XCTAssertEqual(fold.totalCount, 15)
        XCTAssertEqual(sessions.count, fold.totalCount)
    }

    /// No warm-ups at all: no fold label, nothing to expand.
    func testNoFoldLabelWhenNothingIsAWarmup() {
        let fold = SessionsFold(
            [joined(session(id: "real", requests: 5, calls: 1, idleMinutes: 0))], now: now)
        XCTAssertNil(fold.foldLabel)
    }

    /// A single warm-up gets the singular noun.
    func testSingularWarmupLabel() {
        XCTAssertEqual(SessionWarmup.foldLabel(count: 1, cost: nil), "1 warm-up")
    }

    /// No priced session in the fold drops the `$` clause entirely, never
    /// `$0.00` — the same rule every other spend line in this build follows.
    func testFoldLabelDropsSpendWhenNothingWasPriced() {
        XCTAssertEqual(SessionWarmup.foldLabel(count: 3, cost: nil), "3 warm-ups")
    }
}
