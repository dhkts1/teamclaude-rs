import XCTest

@testable import TcrBarCore

/// "Show the running-tool count" (Settings window, Menu Bar pane).
@MainActor
final class ShowRunningToolsPreferenceTests: XCTestCase {

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
        XCTAssertEqual(ShowRunningToolsPreference.key, "showRunningToolCountInMenuBar")
    }

    /// Unlike `MenuBarCountsPreference`, an absent key reads as OFF — this is
    /// a new row, not a preserved default.
    func testAnAbsentKeyReadsAsOff() {
        XCTAssertNil(defaults.object(forKey: ShowRunningToolsPreference.key))
        XCTAssertFalse(ShowRunningToolsPreference(defaults: defaults).showRunningToolCount)
    }

    func testSettingItWritesThroughAndSurvivesANewInstance() {
        let preference = ShowRunningToolsPreference(defaults: defaults)
        preference.showRunningToolCount = true

        XCTAssertTrue(defaults.bool(forKey: ShowRunningToolsPreference.key))
        XCTAssertTrue(ShowRunningToolsPreference(defaults: defaults).showRunningToolCount)
    }
}
