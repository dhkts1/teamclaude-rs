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
/// four names below are `PanelTab`'s own `CaseIterable` labels
/// (`FleetView.swift:3247`), duplicated as literals rather than as a shared
/// type for that boundary reason; ``DefaultTabPreferenceTests`` pins them.
///
/// `peers` used to be absent here on the grounds that the tab drew a
/// placeholder. It draws the real thing now, two switches, a peer per card
/// and a count line, from `tcr peer ls --json`, so it is a tab the panel may
/// open on like any other, and this is the change that header predicted.
///
/// Both executable-side sites know that now. `PanelTab` carries a `String`
/// raw value equal to the names below, the "Open on" picker tags
/// `PanelTab.allCases` (`SettingsPanes.swift`) so `peers` can be selected,
/// and `MenuBarShell.initialTab(from:)` is `PanelTab(rawValue:) ?? .accounts`
///, it used to `switch` over three of the four names, which accepted `peers`
/// here and then opened the Accounts tab anyway. The four strings and
/// `PanelTab`'s cases are one decision in two targets, which is the boundary
/// this type cannot cross; ``DefaultTabPreferenceTests`` pins them here and
/// `PeersPanelWiringTests` pins the enum against this set.
@MainActor
public final class DefaultTabPreference: ObservableObject {
    /// The `UserDefaults` key. Do not change it — the same trap
    /// ``LaunchPreference/startServerAtLaunchKey`` documents.
    public static let key = "defaultPanelTab"

    /// The only four values this preference accepts; anything else in
    /// `UserDefaults` (an old key, a hand edit) is treated as absent.
    public static let validTabs: Set<String> = ["accounts", "sessions", "tools", "peers"]
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
