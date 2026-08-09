import Combine
import Foundation

/// "Start server at launch", persisted in `UserDefaults`.
///
/// ## Why this type exists at all
///
/// It was `@AppStorage("startServerAtLaunch")` on a SwiftUI `App` struct. There
/// is no `App` struct any more — the shell is a hand-managed `NSStatusItem` — and
/// `@AppStorage` is a property wrapper for a `View` or an `App`, so the
/// preference needs somewhere else to live. A plain `UserDefaults` read would not
/// do: nothing would publish the change, so the checkbox in the panel would
/// appear not to move when it was clicked.
///
/// ## The key string is load-bearing
///
/// ``startServerAtLaunchKey`` is exactly the string `@AppStorage` used, and
/// renaming it does not fail anything — it silently resets a preference the
/// operator chose, and does it quietly enough that the first symptom is a proxy
/// that stopped coming up at login. `LaunchPreferenceTests` pins the literal.
///
/// The stored value is deliberately *not* validated or defaulted to anything but
/// `false`: `UserDefaults.bool(forKey:)` returns `false` for an absent key, which
/// is the same default the `@AppStorage` declaration carried, and this option is
/// opt-in on purpose (see `FleetView.startServerToggle` — once TcrBar supervises
/// the server, quitting TcrBar stops it).
@MainActor
public final class LaunchPreference: ObservableObject {

    /// The `UserDefaults` key. Do not change it. See the type's doc-comment.
    public static let startServerAtLaunchKey = "startServerAtLaunch"

    private let defaults: UserDefaults

    /// Written through to `UserDefaults` on every change, so the value the panel
    /// shows and the value the next launch reads cannot disagree.
    @Published public var startServerAtLaunch: Bool {
        didSet { defaults.set(startServerAtLaunch, forKey: Self.startServerAtLaunchKey) }
    }

    /// - Parameter defaults: injected so a test can use a scratch suite rather
    ///   than writing to the operator's real preferences.
    public init(defaults: UserDefaults = .standard) {
        self.defaults = defaults
        // Property initialisation in `init` does not fire `didSet`, so reading
        // the stored value here cannot write it straight back.
        self.startServerAtLaunch = defaults.bool(forKey: Self.startServerAtLaunchKey)
    }
}
