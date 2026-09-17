import Foundation

/// What makes a session a cache-warm stub rather than a session an operator
/// is waiting on. Measured live against the proxy, 2026-09-17:
/// `curl -s http://127.0.0.1:3456/_tcr/status` — 28 sessions, 14 of them
/// matching every clause below, together $2.06. Half the Sessions tab was
/// two requests and then silence, burying the sessions actually doing work.
///
/// All three clauses are load-bearing:
/// - `requests <= maxRequests` and `tools.calls == 0` — it never did
///   anything.
/// - idle for more than ``idleThresholdSeconds`` — WITHOUT this clause the
///   live count was 17, not 14. The extra three were sessions that had just
///   started, which is exactly the moment an operator most wants to see one
///   appear, so a brand-new session must stay visible on its own.
public enum SessionWarmup {
    /// Two requests: enough for a cache-warming round trip, not enough to be
    /// a session anyone is actually driving.
    public static let maxRequests = 2

    /// Five minutes. `>`, not `>=`: a session seen exactly on the boundary is
    /// still fresh enough to show on its own rather than fold into the line
    /// below.
    public static let idleThresholdSeconds: TimeInterval = 5 * 60

    public static func isWarmup(_ session: JoinedSession, now: Date) -> Bool {
        session.session.requests <= maxRequests
            && session.session.tools.calls == 0
            && now.timeIntervalSince(session.lastSeenAt) > idleThresholdSeconds
    }

    /// "`14 warm-ups · $2.06`" — the fold line's label. The spend clause
    /// drops, the same rule ``FleetView/sessionBlockSummary(_:)`` and every
    /// other spend line here follows, when not one folded session was priced.
    public static func foldLabel(count: Int, cost: Double?) -> String {
        let noun = count == 1 ? "warm-up" : "warm-ups"
        guard let cost else { return "\(count) \(noun)" }
        return "\(count) \(noun) · \(QuotaFormat.usd(cost))"
    }
}

/// The Sessions tab split into what's shown directly and what's folded into
/// one muted line at the bottom. Real rows are never destroyed, only
/// grouped — the fold's own ``warmups`` are still full ``JoinedSession``
/// values, ready to draw the moment the line is expanded.
public struct SessionsFold: Equatable {
    public let visible: [JoinedSession]
    public let warmups: [JoinedSession]

    public init(_ sessions: [JoinedSession], now: Date) {
        var visible: [JoinedSession] = []
        var warmups: [JoinedSession] = []
        for session in sessions {
            if SessionWarmup.isWarmup(session, now: now) {
                warmups.append(session)
            } else {
                visible.append(session)
            }
        }
        self.visible = visible
        self.warmups = warmups
    }

    /// Every session that went into this fold — visible plus folded.
    ///
    /// The Sessions tab's header ("28 sessions") is built from this, not from
    /// `visible.count`: folding fourteen sessions into the summary line below
    /// is a DISPLAY choice, not a claim that fourteen sessions stopped
    /// existing. `FleetView.swift`'s own comment on the header's zero case
    /// (guarding `sessionsSupported`) already refuses exactly this failure
    /// mode one clause over — a count the header shows must be a real
    /// measurement, never adjusted to match what happens to be drawn. So the
    /// header keeps counting every session the wire reported; only the ROWS
    /// fold.
    public var totalCount: Int { visible.count + warmups.count }

    public var warmupCost: Double? {
        var spend: Double?
        for session in warmups { spend = UsageTotals.addCost(spend, session.session.costUsd) }
        return spend
    }

    public var foldLabel: String? {
        guard !warmups.isEmpty else { return nil }
        return SessionWarmup.foldLabel(count: warmups.count, cost: warmupCost)
    }
}
