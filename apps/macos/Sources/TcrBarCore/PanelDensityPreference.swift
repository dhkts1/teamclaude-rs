import Combine
import Foundation

/// The two densities `PanelV4`'s tokens offer (`V4.compact`) — `compact`, the
/// shipped default Gil approved from the side-by-side (`data/plans/
/// panel-density-bridge.md`, 2026-09-13: "yes the right compact is better"),
/// and `comfortable`, the original v4 sheet's own numbers kept for anyone who
/// wants the airier spacing back.
public enum PanelDensity: String, CaseIterable, Sendable {
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
/// ``current(defaults:)`` instead, a plain nonisolated `UserDefaults` read,
/// so a density TOKEN (a `static var` evaluated on every draw, including from
/// `RenderStates`) never has to construct an `@MainActor` `ObservableObject`
/// just to size a box.
@MainActor
public final class PanelDensityPreference: ObservableObject {
    /// The `UserDefaults` key. Do not change it — the same trap
    /// ``LaunchPreference/startServerAtLaunchKey`` documents. `nonisolated`
    /// so ``current(defaults:)`` can read it from any context, not only
    /// `@MainActor`.
    public nonisolated static let key = "panelDensity"

    private let defaults: UserDefaults

    @Published public var density: PanelDensity {
        didSet { defaults.set(density.rawValue, forKey: Self.key) }
    }

    public init(defaults: UserDefaults = .standard) {
        self.defaults = defaults
        self.density = Self.current(defaults: defaults)
    }

    /// The stored density, without constructing this class — an absent or
    /// unrecognised value (an old key, a hand edit) reads as `.compact`, the
    /// shipped default, matching `DefaultTabPreference`'s treatment of an
    /// invalid stored value as absent rather than as a crash.
    public nonisolated static func current(defaults: UserDefaults = .standard) -> PanelDensity {
        defaults.string(forKey: key).flatMap(PanelDensity.init(rawValue:)) ?? .compact
    }
}
