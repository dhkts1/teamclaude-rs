import Foundation

/// How much longer a session's prompt cache stays warm, as the Sessions row prints it.
///
/// Anthropic's cache entry expires 5 minutes or 1 hour after the last request that touched it;
/// the first message after that rewrites the whole context at the write price instead of
/// reading it at a tenth. The server knows each session's tier (`Session.cacheTtlSecs`, the
/// tier of its most recent cache write); this turns that and the last request time into the
/// words and the colour role, as a pure function so a test can reach it (the view cannot be
/// instantiated here).
public enum CacheCountdown {
    /// Which colour the row draws the clause in. A role, not a colour: the view maps it onto its
    /// own tokens, so the thresholds stay testable here.
    public enum Tone: Equatable, Sendable {
        /// More than a tenth of the tier is left.
        case normal
        /// The last tenth of the tier.
        case near
        /// The last minute.
        case danger
        /// Already expired.
        case cold
    }

    public struct Reading: Equatable, Sendable {
        public let text: String
        public let tone: Tone
    }

    /// `nil` when the server sent no tier: a session that has never written the cache, or a
    /// server built before the field existed. No clause is drawn then; an unknown is never
    /// read as cold.
    ///
    /// `cache 59m` above a minute (whole minutes, rounded down, so it never promises time that
    /// is gone), `cache 40s` under it, `cache cold` at or past zero.
    public static func reading(lastSeenMs: Int64, ttlSecs: Int?, now: Date) -> Reading? {
        guard let ttl = ttlSecs, ttl > 0 else { return nil }
        let nowMs = Int64((now.timeIntervalSince1970 * 1000).rounded())
        let remaining = Int((lastSeenMs + Int64(ttl) * 1000 - nowMs) / 1000)
        if remaining <= 0 { return Reading(text: "cache cold", tone: .cold) }
        let tone: Tone
        if remaining <= 60 {
            tone = .danger
        } else if remaining * 10 <= ttl {
            tone = .near
        } else {
            tone = .normal
        }
        let text = remaining < 60 ? "cache \(remaining)s" : "cache \(remaining / 60)m"
        return Reading(text: text, tone: tone)
    }
}
