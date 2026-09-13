import XCTest

@testable import TcrBarCore

/// "Panel density" (Settings window, Menu Bar pane → "When the panel opens").
/// Same coverage shape as `ShowRunningToolsPreferenceTests`.
@MainActor
final class PanelDensityPreferenceTests: XCTestCase {

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
        XCTAssertEqual(PanelDensityPreference.key, "panelDensity")
    }

    /// An absent key reads as `.compact` — the shipped default Gil approved
    /// from the side-by-side, not the pre-existing behaviour being preserved.
    func testAnAbsentKeyReadsAsCompact() {
        XCTAssertNil(defaults.object(forKey: PanelDensityPreference.key))
        XCTAssertEqual(PanelDensityPreference(defaults: defaults).density, .compact)
        XCTAssertEqual(PanelDensityPreference.current(defaults: defaults), .compact)
    }

    /// An unrecognised stored value (an old key, a hand edit) reads as absent
    /// — `.compact` — the same treatment `DefaultTabPreference` gives an
    /// invalid stored tab, rather than a crash or an unrelated third case.
    func testAnUnrecognisedStoredValueReadsAsCompact() {
        defaults.set("roomy", forKey: PanelDensityPreference.key)
        XCTAssertEqual(PanelDensityPreference(defaults: defaults).density, .compact)
    }

    func testSettingItWritesThroughAndSurvivesANewInstance() {
        let preference = PanelDensityPreference(defaults: defaults)
        preference.density = .comfortable

        XCTAssertEqual(defaults.string(forKey: PanelDensityPreference.key), "comfortable")
        XCTAssertEqual(PanelDensityPreference(defaults: defaults).density, .comfortable)
        XCTAssertEqual(PanelDensityPreference.current(defaults: defaults), .comfortable)
    }

    func testSettingItBackToCompactWritesThrough() {
        defaults.set("comfortable", forKey: PanelDensityPreference.key)
        let preference = PanelDensityPreference(defaults: defaults)
        XCTAssertEqual(preference.density, .comfortable)

        preference.density = .compact
        XCTAssertEqual(defaults.string(forKey: PanelDensityPreference.key), "compact")
        XCTAssertEqual(PanelDensityPreference.current(defaults: defaults), .compact)
    }
}
