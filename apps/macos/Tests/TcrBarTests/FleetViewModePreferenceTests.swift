import XCTest

@testable import TcrBarCore

/// Same shape and same reasons as `LaunchPreferenceTests`: no `@AppStorage`
/// available outside a SwiftUI `View`/`App`, so this is a plain
/// `UserDefaults`-backed `ObservableObject` instead.
@MainActor
final class FleetViewModePreferenceTests: XCTestCase {

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

    func testDefaultsToAccounts() {
        XCTAssertEqual(FleetViewModePreference(defaults: defaults).mode, .accounts)
    }

    func testAStoredValueIsRead() {
        defaults.set("groups", forKey: FleetViewModePreference.modeKey)
        XCTAssertEqual(FleetViewModePreference(defaults: defaults).mode, .groups)
    }

    /// An unrecognised stored value — a future case this build does not know,
    /// or corrupted defaults — must fall back to `.accounts` rather than
    /// crash or read as `nil`.
    func testUnrecognisedStoredValueFallsBackToAccounts() {
        defaults.set("nonsense", forKey: FleetViewModePreference.modeKey)
        XCTAssertEqual(FleetViewModePreference(defaults: defaults).mode, .accounts)
    }

    func testSettingItWritesThroughAndSurvivesANewInstance() {
        let preference = FleetViewModePreference(defaults: defaults)
        preference.mode = .groups

        XCTAssertEqual(defaults.string(forKey: FleetViewModePreference.modeKey), "groups")
        XCTAssertEqual(FleetViewModePreference(defaults: defaults).mode, .groups)

        preference.mode = .accounts
        XCTAssertEqual(defaults.string(forKey: FleetViewModePreference.modeKey), "accounts")
    }

    func testConstructingItDoesNotWriteTheKey() {
        _ = FleetViewModePreference(defaults: defaults)
        XCTAssertNil(defaults.object(forKey: FleetViewModePreference.modeKey))
    }
}
