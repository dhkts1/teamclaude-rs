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
    /// `"45s"`, `"4m 12s"` — no day tier: the longest call this tab shows is
    /// the Bash tool's own timeout, six orders of magnitude under a day.
    public static func duration(_ seconds: Double) -> String {
        let total = Int(seconds.rounded())
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
}
