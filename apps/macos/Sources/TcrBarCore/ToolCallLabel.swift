/// What the Tools tab says about one call's duration — in print and out loud.
///
/// Here rather than in `FleetView` because the test target links `TcrBarCore`
/// only (`Package.swift:39-43`): a string a view composes privately is a string
/// no assertion can read, and the whole point of this type is that the two
/// halves of it — what a sighted reader sees and what VoiceOver speaks — are
/// pinned to the same facts by a test.
///
/// The defect it exists for (review finding 4): a call KILLED at the Bash tool's 600 s timeout and one that
/// merely ran long drew the identical string in the identical pill, red against
/// grey the only difference. Measured on `17-tools-tab-dark.png`: `10M 0S` in
/// `#ea5a56`, `9M 43S` in `#bfbeb9`, same size, same shape. State carried by
/// colour alone, and the state in question is the most actionable fact on the
/// tab.
public enum ToolCallLabel {
    /// `"45s"`, `"4m 12s"`, `"3h 0m"`.
    ///
    /// The hour tier exists because the premise this function shipped with was
    /// wrong. It used to read "no day tier: the longest call this tab shows is
    /// the Bash tool's own timeout, six orders of magnitude under a day", and
    /// that is only true of a CAPPED call. RUNNING NOW also lists uncapped
    /// ones — an `Agent`, a `TaskOutput` — which have no deadline at all and
    /// routinely run for hours. Rendered by the old two-tier form, a three
    /// hour subagent printed `180m 3s`, which a reader has to divide to
    /// understand, and on the Sessions tab's narrower "oldest" column it did
    /// not even fit: it truncated to `180m…`, losing the unit.
    ///
    /// Nothing under an hour changes, which is every capped call this tab can
    /// show. Above it the seconds are dropped rather than carried, the same
    /// choice ``HeldWindow/duration(minutes:)`` already makes at its own scale
    /// (`4d 12h`, never `4d 12h 30m`): at three hours a second is not a fact
    /// anyone reads, and the column is 96pt.
    ///
    /// Still no DAY tier, and now for a reason that survives: a call running
    /// past 24 hours is older than ``SESSION_TTL_MS`` and older than the
    /// six-hour lost-result backstop, so the wire has dropped it long before
    /// it could reach this formatter.
    public static func duration(_ seconds: Double) -> String {
        let total = Int(seconds.rounded())
        if total >= 3600 {
            return "\(total / 3600)h \((total % 3600) / 60)m"
        }
        let minutes = total / 60
        let rest = total % 60
        return minutes > 0 ? "\(minutes)m \(rest)s" : "\(rest)s"
    }

    /// Was this call killed at the timeout, rather than merely slow?
    ///
    /// `>=`, not `==`: the wire reports the call's own elapsed seconds and a
    /// kill lands a hair past the deadline it was measured against.
    public static func timedOut(seconds: Double, timeout: Double) -> Bool {
        seconds >= timeout
    }

    /// The pill's printed text.
    ///
    /// A killed call spends the pill on the WORD rather than the number, and
    /// that is a deliberate trade of one fact for the other: the number is
    /// already known for this case (it is the timeout, which the summary line
    /// above states in full — "31 hit the 600s timeout"), while "this one was
    /// killed" is said nowhere else on the row. The exact elapsed time is not
    /// lost — ``spoken(seconds:timeout:)`` carries it to VoiceOver and to the
    /// row's tooltip.
    ///
    /// Measured before choosing it: the pill's glyph run is about 6.9 pt a
    /// character at this size, so `10m 0s · timed out` needs ~138 pt against a
    /// 60 pt column, and widening the column that far takes 78 pt from the
    /// command the row is about. `timed out` alone needs ~76 pt, which fits
    /// `V4.trailingColumnWidth` (96 pt), the width this tab already declares.
    public static func pill(seconds: Double, timeout: Double) -> String {
        timedOut(seconds: seconds, timeout: timeout) ? "timed out" : duration(seconds)
    }

    /// The pill's accessibility value, and the same sentence as its tooltip —
    /// `nil` for a call that simply ran, whose pill already speaks its duration.
    public static func spoken(seconds: Double, timeout: Double) -> String? {
        guard timedOut(seconds: seconds, timeout: timeout) else { return nil }
        return "ran \(duration(seconds)), killed at the \(Int(timeout)) second timeout"
    }

    /// What a RUNNING NOW row prints beside its ring: `"4m 12s"`, or
    /// `"9m 40s · 20s left"` once the call is inside the warning band.
    ///
    /// `remaining` is `nil` for a call with no known cap — an Agent, a Read —
    /// and such a call can never print a "left" clause, because there is
    /// nothing this build knows it is running out of. That is the same refusal
    /// the ring makes by not being drawn at all for those tools; stating it
    /// once, here, is what stops the two from disagreeing (they did: the first
    /// render of this layout printed "9m 43s · 17s left" beside an Agent call
    /// with no ring).
    ///
    /// The clause says "left" rather than "to timeout" because the row is
    /// already under a section head that states the 600s timeout out loud, and
    /// the shorter word fits beside the elapsed time in the trailing column.
    public static func running(elapsed: Double?, remaining: Double?, warnWithin: Double) -> String {
        let age = elapsed.map(duration) ?? ""
        guard let remaining, remaining <= warnWithin else { return age }
        return "\(age) · \(Int(max(0, remaining.rounded())))s left"
    }
}
