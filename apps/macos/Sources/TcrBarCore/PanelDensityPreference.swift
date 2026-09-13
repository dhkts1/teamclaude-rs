import Combine
import Foundation

/// What the operator chose in Settings → Menu Bar → "Panel density".
///
/// Three values, and only two of them are a density: `auto` is a RULE, and it
/// is the shipped default (Gil, 2026-09-13: "make compact the default please
/// above 4 accounts"). `compact` and `comfortable` are the two manual
/// overrides — `compact` is the spacing Gil approved from the side-by-side
/// (`data/plans/panel-density-bridge.md`, "yes the right compact is better"),
/// `comfortable` the original v4 sheet's own numbers.
///
/// Resolving `auto` is ``PanelDensityPreference/resolved(defaults:accounts:)``
/// and nothing else: a token may not ask "is the preference `.compact`"
/// anymore, because on the default setting the answer is neither yes nor no.
public enum PanelDensity: String, CaseIterable, Sendable {
    case auto
    case compact
    case comfortable
}

/// The two densities the `V4` tokens actually shrink for, with ``PanelDensity``
/// `.auto` already decided.
///
/// A separate type rather than a `PanelDensity` that "should not be `.auto` by
/// now": the resolver returns everything it learned, so no caller downstream
/// can be handed a value it still has to interpret, and no second place can
/// interpret it differently.
public enum ResolvedPanelDensity: String, Equatable, Sendable {
    case compact
    case comfortable
}

/// "Panel density" (Settings window, Menu Bar pane → "When the panel opens").
/// Same shape as ``ShowRunningToolsPreference``, and the same reason: there
/// is no `App`/`View` to hang an `@AppStorage` off, and a plain
/// `UserDefaults` read would not publish the change back to the picker that
/// set it.
///
/// `PanelV4.V4.compact` does not hold an instance of this class — it reads
/// ``resolved(defaults:accounts:)`` instead, a plain nonisolated read, so a
/// density TOKEN (a `static var` evaluated on every draw, including from
/// `RenderStates`) never has to construct an `@MainActor` `ObservableObject`
/// just to size a box.
@MainActor
public final class PanelDensityPreference: ObservableObject {
    /// The `UserDefaults` key. Do not change it — the same trap
    /// ``LaunchPreference/startServerAtLaunchKey`` documents. `nonisolated`
    /// so ``current(defaults:)`` can read it from any context, not only
    /// `@MainActor`.
    public nonisolated static let key = "panelDensity"

    /// The largest fleet that still draws Comfortable under `.auto`.
    ///
    /// Four, from Gil's own words ("above 4 accounts"), and it is the ceiling
    /// rather than the floor: 4 accounts is Comfortable, 5 is Compact. A
    /// constant and not a literal in the comparison because the Settings row
    /// names the same number out loud ("Automatic (compact above 4
    /// accounts)"), and a rule written in two places drifts.
    public nonisolated static let comfortableCeiling = 4

    private let defaults: UserDefaults

    @Published public var density: PanelDensity {
        didSet { defaults.set(density.rawValue, forKey: Self.key) }
    }

    public init(defaults: UserDefaults = .standard) {
        self.defaults = defaults
        self.density = Self.current(defaults: defaults)
    }

    /// The stored preference, without constructing this class — an absent or
    /// unrecognised value (an old key, a hand edit) reads as `.auto`, the
    /// shipped default, matching `DefaultTabPreference`'s treatment of an
    /// invalid stored value as absent rather than as a crash.
    ///
    /// A pre-`auto` install that stored `"compact"` when compact WAS the
    /// default keeps compact: the value is still valid, so the operator's
    /// last visible choice survives this change rather than being silently
    /// re-decided for them.
    public nonisolated static func current(defaults: UserDefaults = .standard) -> PanelDensity {
        defaults.string(forKey: key).flatMap(PanelDensity.init(rawValue:)) ?? .auto
    }

    /// The density to draw at: the manual choice when there is one, otherwise
    /// the fleet's own size against ``comfortableCeiling``.
    ///
    /// `accounts` is EVERY row a person sees, parked and disabled included —
    /// the question the rule answers is "does this panel have a lot in it",
    /// and a disabled account costs exactly as much height as a serving one.
    /// `nil` is "no fleet yet": a panel with nothing decoded draws Comfortable,
    /// because the not-a-fleet states are a banner and a button, never a list.
    public nonisolated static func resolved(
        defaults: UserDefaults = .standard,
        accounts: Int? = PanelDensityPreference.accountCount
    ) -> ResolvedPanelDensity {
        switch current(defaults: defaults) {
        case .compact: return .compact
        case .comfortable: return .comfortable
        case .auto: return (accounts ?? 0) > comfortableCeiling ? .compact : .comfortable
        }
    }

    // MARK: - The fleet's size, as the tokens see it

    private nonisolated(unsafe) static var storedAccountCount: Int?
    private nonisolated static let accountCountLock = NSLock()

    /// How many account rows the panel is currently drawing, or `nil` before
    /// anything has decoded.
    ///
    /// A static box rather than a parameter threaded down the view tree: every
    /// `V4` token is a `static var` read during layout by views that have no
    /// fleet in scope (`PanelHeader`, `PanelFooter`, `V4Card`), and giving each
    /// of them an account count to pass along would put the same number in
    /// thirty signatures. ``PanelV4`` sets it at the top of its `body`, before
    /// it returns the tree, so it is already current by the time the first
    /// token in that tree is read.
    public nonisolated static var accountCount: Int? {
        accountCountLock.lock()
        defer { accountCountLock.unlock() }
        return storedAccountCount
    }

    /// Records the fleet's size for ``resolved(defaults:accounts:)``. Safe to
    /// call on every draw: it writes a number and publishes nothing, so it
    /// cannot itself invalidate the view that called it.
    public nonisolated static func setAccountCount(_ count: Int?) {
        accountCountLock.lock()
        storedAccountCount = count
        accountCountLock.unlock()
    }
}
