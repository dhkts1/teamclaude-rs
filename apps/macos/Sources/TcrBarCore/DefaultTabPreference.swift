import Combine
import Foundation

/// "Open on" — which panel tab the popover selects the next time it opens.
/// Same shape as ``MenuBarCountsPreference``, for the same reason: there is no
/// `App`/`View` to hang an `@AppStorage` off, and a plain `UserDefaults` read
/// would not publish the change back to the picker that set it.
///
/// Stores a plain string rather than `PanelTab` — that enum lives in the
/// `TcrBar` executable target (`FleetView.swift`) so it can stay unaware of
/// Sparkle, and `TcrBarCore` cannot import upward from its own client. The
/// three names below are `PanelTab`'s own `CaseIterable` labels
/// (`FleetView.swift:1727`), duplicated as literals rather than as a shared
/// type for that boundary reason; ``DefaultTabPreferenceTests`` pins them.
@MainActor
public final class DefaultTabPreference: ObservableObject {
    /// The `UserDefaults` key. Do not change it — the same trap
    /// ``LaunchPreference/startServerAtLaunchKey`` documents.
    public static let key = "defaultPanelTab"

    /// The only three values this preference accepts; anything else in
    /// `UserDefaults` (an old key, a hand edit) is treated as absent.
    public static let validTabs: Set<String> = ["accounts", "sessions", "tools"]
    public static let fallback = "accounts"

    private let defaults: UserDefaults

    @Published public var tab: String {
        didSet { defaults.set(tab, forKey: Self.key) }
    }

    public init(defaults: UserDefaults = .standard) {
        self.defaults = defaults
        let stored = defaults.string(forKey: Self.key)
        self.tab = stored.flatMap { Self.validTabs.contains($0) ? $0 : nil } ?? Self.fallback
    }
}
