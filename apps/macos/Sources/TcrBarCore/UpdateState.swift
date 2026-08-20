import Foundation

/// What Sparkle's last update check actually found — not a `Bool`, because
/// this codebase's whole idiom is that "no" and "don't know" are different
/// facts. `.unknown` is the honest value before any check has completed;
/// rendering that as "up to date" would be the same class of lie as an
/// unmeasured `0` (see the doc-comments in `FleetStatus.swift`).
///
/// This type carries no Sparkle types on purpose: `TcrBarCore` stays
/// framework-free (`Package.swift` documents why — the test target links
/// only this library), so the `TcrBar` executable's `Updater` extracts plain
/// values (a version string, an error's message) out of Sparkle's delegate
/// callbacks and hands them to this enum. That boundary is also why this
/// state's own semantics — what each case means, and what the header shows
/// for it — are unit-testable, while the delegate methods that populate it
/// are not: see ``headerMessage``.
public enum UpdateState: Equatable, Sendable {
    /// No check has completed yet. The starting value, and the only honest
    /// one before Sparkle has said anything at all.
    case unknown
    /// The last completed check found nothing newer than what is running.
    case upToDate
    /// The last completed check found a newer release, reported by Sparkle's
    /// own `didFindValidUpdate` callback — never inferred by comparing
    /// version strings here.
    case available(version: String)
    /// The last check could not complete — Sparkle's `didAbortWithError`,
    /// carrying its message verbatim so a silently broken feed is visible.
    case failed(String)

    /// What the panel header shows for this state, or `nil` to show nothing.
    /// `.unknown` and `.upToDate` render nothing — this panel is 380pt wide
    /// and does not get a permanent "you're up to date" row; `.available`
    /// and `.failed` are the only two facts worth a line, because both are
    /// something the user should act on or at least notice.
    public var headerMessage: String? {
        switch self {
        case .unknown, .upToDate:
            return nil
        case .available(let version):
            return "Update available: \(version)"
        case .failed(let reason):
            return "Update check failed: \(reason)"
        }
    }
}
