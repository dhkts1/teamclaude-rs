import Foundation

/// The Sessions tab's list, cut by PROJECT rather than account.
///
/// Measured on the live fleet (the bridge that asked for this): by account
/// the biggest bucket holds 90% of sessions — Account is already the whole
/// job of the Accounts tab — while by project the split is 50/20/20. Grouping
/// by project is the axis that actually separates the fleet into meaningful
/// pieces.
///
/// Lives here, not in `FleetView`, for the reason ``FleetSections`` already
/// gives for the Accounts list: the test target links `TcrBarCore` only, and
/// grouping/ordering is exactly the kind of rule that silently drifts when a
/// view recomputes it inline.

/// One project heading, or the absence of one — the inner grouping key for
/// the Sessions tab. Not a bare `String`: a session with no `cwd` on its
/// session file (``JoinedSession/project`` `nil`) is a distinct fact from a
/// project literally named "Unassigned", and a string would let the two
/// collide.
public enum SessionProjectKey: Hashable, Sendable {
    case named(String)
    case unassigned

    public init(_ session: JoinedSession) {
        if let project = session.project, !project.isEmpty {
            self = .named(project)
        } else {
            self = .unassigned
        }
    }

    public var title: String {
        switch self {
        case .named(let name): return name
        case .unassigned: return "Unassigned"
        }
    }
}

/// One project's sessions, already ordered — the caller renders `rows` in
/// array order and derives nothing further.
public struct SessionProjectSection: Identifiable, Equatable, Sendable {
    public let key: SessionProjectKey
    public let rows: [JoinedSession]

    public init(key: SessionProjectKey, rows: [JoinedSession]) {
        self.key = key
        self.rows = rows
    }

    public var id: String {
        switch key {
        case .named(let name): return "p:\(name)"
        case .unassigned: return "p:"
        }
    }

    /// **A project with fewer than two sessions draws no header at all** — its
    /// one session becomes an ordinary row with a dim project tag instead
    /// (`docs/design/panel-tabs.md` §3: "One header describing one trivial
    /// row costs more than it explains"). This is the property both
    /// ``SessionSections/byProject(_:)`` (which decides row order) and
    /// `FleetView`'s renderer (which decides whether to draw a header at
    /// all) read, so the two cannot draw a header over a lone row that the
    /// count itself says should not have one.
    public var hasHeader: Bool { rows.count >= 2 }

    /// What this section spent, summed the same partial-sum way
    /// ``FleetSection/todaySpend`` sums an account section's cost: `nil` only
    /// when not one row could be priced, never a fabricated zero.
    public var spend: Double? {
        var total: Double?
        for row in rows {
            total = UsageTotals.addCost(total, row.session.costUsd)
        }
        return total
    }

    /// Every error this section's sessions have logged today
    /// (``SessionTools/errors``, already on the wire — nothing new is added
    /// for this).
    public var errors: Int {
        rows.reduce(0) { $0 + $1.session.tools.errors }
    }

    /// "5 · $976 · 166 err" — a headed section's trailing summary, drawn
    /// beside its project name. The error clause drops when there is nothing
    /// to report, the same "no clause over a genuine zero-as-unmeasured"
    /// posture every other summary in this file takes — though here a zero
    /// really is zero errors, so it is dropped for BREVITY, not honesty: the
    /// count and the spend already say the section is real.
    public var headerSummary: String {
        var parts = ["\(rows.count)"]
        if let spend { parts.append(QuotaFormat.usd(spend)) }
        if errors > 0 { parts.append("\(errors) err") }
        return parts.joined(separator: " · ")
    }
}

public enum SessionSections {
    /// Groups the joined sessions by project, ordered by section spend —
    /// highest first, per `docs/design/panel-tabs.md` §3 — with a section
    /// carrying no priced session sorting after every priced one rather than
    /// claiming a `$0` spend it never measured.
    ///
    /// Row order WITHIN a section is preserved from `sessions`' own order —
    /// ``FleetView.sessionsList`` already sorts sessions before joining them,
    /// and re-sorting here would be a second place that could rank them
    /// differently.
    public static func byProject(_ sessions: [JoinedSession]) -> [SessionProjectSection] {
        var order: [SessionProjectKey] = []
        var buckets: [SessionProjectKey: [JoinedSession]] = [:]
        for session in sessions {
            let key = SessionProjectKey(session)
            if buckets[key] == nil { order.append(key) }
            buckets[key, default: []].append(session)
        }
        return
            order
            .map { SessionProjectSection(key: $0, rows: buckets[$0] ?? []) }
            .sorted { lhs, rhs in
                switch (lhs.spend, rhs.spend) {
                case (let a?, let b?): return a > b
                case (nil, nil): return false
                case (nil, _): return false
                case (_, nil): return true
                }
            }
    }
}
