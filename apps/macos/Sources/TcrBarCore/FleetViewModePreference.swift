import Combine
import Foundation

/// Which of the panel's two lists is showing — `Accounts` or `Groups` —
/// persisted in `UserDefaults`.
///
/// Same shape as ``LaunchPreference`` and for the same reason: there is no
/// SwiftUI `App`/`View` to hang an `@AppStorage` off (the shell is a
/// hand-managed `NSStatusItem`, see ``LaunchPreference``'s own doc-comment),
/// and a plain `UserDefaults` read publishes nothing, so the toggle would
/// appear not to move when clicked.
@MainActor
public final class FleetViewModePreference: ObservableObject {
    /// The `UserDefaults` key. Do not change it — renaming silently resets a
    /// choice the operator made, the same failure mode
    /// ``LaunchPreference/startServerAtLaunchKey``'s doc-comment names.
    public static let modeKey = "fleetViewMode"

    public enum Mode: String, Equatable, Sendable {
        case accounts
        case groups
    }

    private let defaults: UserDefaults

    /// Written through to `UserDefaults` on every change.
    @Published public var mode: Mode {
        didSet { defaults.set(mode.rawValue, forKey: Self.modeKey) }
    }

    /// - Parameter defaults: injected so a test can use a scratch suite
    ///   rather than writing to the operator's real preferences.
    public init(defaults: UserDefaults = .standard) {
        self.defaults = defaults
        // Property initialisation in `init` does not fire `didSet`, so
        // reading the stored value here cannot write it straight back.
        if let raw = defaults.string(forKey: Self.modeKey), let stored = Mode(rawValue: raw) {
            self.mode = stored
        } else {
            self.mode = .accounts
        }
    }
}
