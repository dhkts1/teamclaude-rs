import XCTest

@testable import TcrBarCore

/// "Open on" (Settings window, Menu Bar pane — `data/plans/
/// settings-window-bridge.md`). Same scratch-suite shape as
/// `MenuBarCountsPreferenceTests`.
@MainActor
final class DefaultTabPreferenceTests: XCTestCase {

    private var suiteName = ""
    private var defaults = UserDefaults.standard

    override func setUp() {
        super.setUp()
        suiteName = "tcrbar.tests.\(UUID().uuidString)"
        defaults = UserDefaults(suiteName: suiteName) ?? .standard
    }

    override func tearDown() {
        UserDefaults.standard.removePersistentDomain(forName: suiteName)
        super.tearDown()
    }

    func testTheDefaultsKeyIsPinned() {
        XCTAssertEqual(DefaultTabPreference.key, "defaultPanelTab")
    }

    /// The three names must match `PanelTab`'s own cases
    /// (`FleetView.swift:1727`) — this is the boundary the type's own
    /// doc-comment explains, so a rename on either side that is not mirrored
    /// on the other is exactly what this test exists to catch.
    func testValidTabsMatchPanelTabsCases() {
        XCTAssertEqual(DefaultTabPreference.validTabs, ["accounts", "sessions", "tools"])
    }

    func testAnAbsentKeyFallsBackToAccounts() {
        XCTAssertNil(defaults.object(forKey: DefaultTabPreference.key))
        XCTAssertEqual(DefaultTabPreference(defaults: defaults).tab, "accounts")
    }

    func testAStoredValidValueIsRead() {
        defaults.set("tools", forKey: DefaultTabPreference.key)
        XCTAssertEqual(DefaultTabPreference(defaults: defaults).tab, "tools")
    }

    /// A stray or stale value (an old key format, a hand edit) must not crash
    /// or propagate — it falls back the same way an absent key does.
    func testAnInvalidStoredValueFallsBackToAccounts() {
        defaults.set("groups", forKey: DefaultTabPreference.key)
        XCTAssertEqual(DefaultTabPreference(defaults: defaults).tab, "accounts")
    }

    func testSettingItWritesThroughAndSurvivesANewInstance() {
        let preference = DefaultTabPreference(defaults: defaults)
        preference.tab = "sessions"

        XCTAssertEqual(defaults.string(forKey: DefaultTabPreference.key), "sessions")
        XCTAssertEqual(DefaultTabPreference(defaults: defaults).tab, "sessions")
    }
}
