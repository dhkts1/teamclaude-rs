import Combine
import Foundation

/// "Show the running-tool count" — a second number beside the menu-bar glyph,
/// alongside ``MenuBarCountsPreference``'s ready/enabled fraction. Same shape,
/// same reason there is no `@AppStorage` to reach for.
///
/// Wired into `MenuBarShell.updateMark`:
/// the segment it gates only ever draws when this is `true` AND the poll's
/// payload actually carries `sessions` (``PollState/runningToolsCount(showRunningTools:)``)
/// — a live server that has not grown the wire yet leaves the mark exactly as
/// it was before this preference existed.
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
