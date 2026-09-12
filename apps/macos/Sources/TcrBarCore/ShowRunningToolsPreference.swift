import Combine
import Foundation

/// "Show the running-tool count" — a second number beside the menu-bar glyph,
/// alongside ``MenuBarCountsPreference``'s ready/enabled fraction. Same shape,
/// same reason there is no `@AppStorage` to reach for.
///
/// **Not yet wired to the glyph.** `MenuBarShell.updateMark` composes the
/// ready/enabled label from ``MenuBarCountsPreference`` and the running-tool
/// count from a live poll is not threaded through it — this preference exists
/// so the Settings window row from the bridge (`data/plans/
/// settings-window-bridge.md` § Panes, "Menu Bar") is real and persists,
/// without also rewriting `updateMark`'s composition, which touches a
/// different feature's tests than this window's own. Reading it back always
/// answers correctly; the mark itself does not consult it yet.
@MainActor
public final class ShowRunningToolsPreference: ObservableObject {
    /// The `UserDefaults` key. Do not change it — the same trap
    /// ``LaunchPreference/startServerAtLaunchKey`` documents.
    public static let key = "showRunningToolCountInMenuBar"

    private let defaults: UserDefaults

    /// Defaults to `false`, matching the mockup's own unchecked state for
    /// this row (`docs/design/panel-tabs-mockup.html`, `aria-checked="false"`
    /// on "Show the running-tool count") — unlike ``MenuBarCountsPreference``,
    /// this one is not the pre-existing behaviour being preserved.
    @Published public var showRunningToolCount: Bool {
        didSet { defaults.set(showRunningToolCount, forKey: Self.key) }
    }

    public init(defaults: UserDefaults = .standard) {
        self.defaults = defaults
        self.showRunningToolCount = defaults.bool(forKey: Self.key)
    }
}
