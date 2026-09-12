import Combine
import Foundation

/// "Show counts in the menu bar" — whether the `ready/enabled` label is drawn
/// beside the gauge glyph. Persisted in `UserDefaults`, same shape as
/// ``LaunchPreference``, for the same reason: there is no `App`/`View` to hang
/// an `@AppStorage` off, and a plain read would not publish the change back to
/// the checkbox that set it.
///
/// ## Default is ON, unlike ``LaunchPreference``
///
/// `UserDefaults.bool(forKey:)` answers `false` for an absent key, which is
/// exactly wrong here: the feature this preference gates should be visible the
/// first time TcrBar runs with it, not opt-in. So the stored value is only
/// trusted when the key has actually been written — `object(forKey:) != nil`
/// — and an absent key reads as `true` instead of falling through to the
/// framework's own `false`.
@MainActor
public final class MenuBarCountsPreference: ObservableObject {

    /// The `UserDefaults` key. Do not change it — the same trap
    /// ``LaunchPreference/startServerAtLaunchKey`` documents: renaming it
    /// fails nothing and silently resets an operator's choice.
    /// `MenuBarCountsPreferenceTests` pins the literal.
    public static let showCountsKey = "showCountsInMenuBar"

    private let defaults: UserDefaults

    /// Written through to `UserDefaults` on every change, so the value the
    /// panel shows and the value the next launch reads cannot disagree.
    @Published public var showCounts: Bool {
        didSet { defaults.set(showCounts, forKey: Self.showCountsKey) }
    }

    /// - Parameter defaults: injected so a test can use a scratch suite rather
    ///   than writing to the operator's real preferences.
    public init(defaults: UserDefaults = .standard) {
        self.defaults = defaults
        // Property initialisation in `init` does not fire `didSet`, so reading
        // the stored value here cannot write it straight back.
        self.showCounts =
            defaults.object(forKey: Self.showCountsKey) == nil
            ? true
            : defaults.bool(forKey: Self.showCountsKey)
    }
}
