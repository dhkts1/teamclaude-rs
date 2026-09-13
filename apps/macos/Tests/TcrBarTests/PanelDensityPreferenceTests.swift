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

    /// An absent key reads as `.auto` — the shipped default since Gil's "make
    /// compact the default please above 4 accounts". These two assertions read
    /// `.compact` until then, which was the previous shipped default and is now
    /// one of the two manual overrides; the expectation moved with the default,
    /// it did not stop being checked.
    func testAnAbsentKeyReadsAsAuto() {
        XCTAssertNil(defaults.object(forKey: PanelDensityPreference.key))
        XCTAssertEqual(PanelDensityPreference(defaults: defaults).density, .auto)
        XCTAssertEqual(PanelDensityPreference.current(defaults: defaults), .auto)
    }

    /// An unrecognised stored value (an old key, a hand edit) reads as absent
    /// — `.auto` — the same treatment `DefaultTabPreference` gives an
    /// invalid stored tab, rather than a crash or an unrelated third case.
    func testAnUnrecognisedStoredValueReadsAsAuto() {
        defaults.set("roomy", forKey: PanelDensityPreference.key)
        XCTAssertEqual(PanelDensityPreference(defaults: defaults).density, .auto)
    }

    /// An install from before `.auto` existed stored `"compact"` back when
    /// compact WAS the default. That value is still valid, so it survives: the
    /// operator's last visible choice is not silently re-decided for them.
    func testAStoredCompactFromBeforeAutoExistedIsKept() {
        defaults.set("compact", forKey: PanelDensityPreference.key)
        XCTAssertEqual(PanelDensityPreference.current(defaults: defaults), .compact)
        XCTAssertEqual(
            PanelDensityPreference.resolved(defaults: defaults, accounts: 2), .compact)
    }

    // MARK: - `.auto` resolves against the fleet's size

    /// The rule, at its boundary: "compact above 4 accounts" makes 4
    /// Comfortable and 5 Compact.
    func testAutoDrawsComfortableAtFourAccounts() {
        XCTAssertEqual(PanelDensityPreference.current(defaults: defaults), .auto)
        XCTAssertEqual(
            PanelDensityPreference.resolved(defaults: defaults, accounts: 4), .comfortable)
    }

    func testAutoDrawsCompactAtFiveAccounts() {
        XCTAssertEqual(
            PanelDensityPreference.resolved(defaults: defaults, accounts: 5), .compact)
    }

    func testTheCeilingIsTheNumberTheSettingsRowNames() {
        XCTAssertEqual(PanelDensityPreference.comfortableCeiling, 4)
        XCTAssertEqual(
            PanelDensityPreference.resolved(
                defaults: defaults, accounts: PanelDensityPreference.comfortableCeiling),
            .comfortable)
        XCTAssertEqual(
            PanelDensityPreference.resolved(
                defaults: defaults, accounts: PanelDensityPreference.comfortableCeiling + 1),
            .compact)
    }

    /// No fleet decoded yet. The not-a-fleet states are a banner and a button,
    /// never a list, so there is nothing for compact to save.
    func testAutoWithNoFleetYetDrawsComfortable() {
        XCTAssertEqual(
            PanelDensityPreference.resolved(defaults: defaults, accounts: nil), .comfortable)
    }

    // MARK: - A manual choice overrides the rule in both directions

    func testManualCompactStaysCompactOnASmallFleet() {
        defaults.set("compact", forKey: PanelDensityPreference.key)
        XCTAssertEqual(
            PanelDensityPreference.resolved(defaults: defaults, accounts: 1), .compact,
            "one account would be Comfortable under .auto; the operator said compact")
    }

    func testManualComfortableStaysComfortableOnALargeFleet() {
        defaults.set("comfortable", forKey: PanelDensityPreference.key)
        XCTAssertEqual(
            PanelDensityPreference.resolved(defaults: defaults, accounts: 13), .comfortable,
            "thirteen accounts would be Compact under .auto; the operator said comfortable")
    }

    /// The static box `PanelV4` writes on every draw, which is what
    /// `V4.compact` reads when no count is passed.
    func testTheRecordedAccountCountIsWhatTheTokensResolveAgainst() {
        defaults.set(PanelDensity.auto.rawValue, forKey: PanelDensityPreference.key)
        let previous = PanelDensityPreference.accountCount
        defer { PanelDensityPreference.setAccountCount(previous) }

        PanelDensityPreference.setAccountCount(13)
        XCTAssertEqual(PanelDensityPreference.accountCount, 13)
        XCTAssertEqual(PanelDensityPreference.resolved(defaults: defaults), .compact)

        PanelDensityPreference.setAccountCount(2)
        XCTAssertEqual(PanelDensityPreference.resolved(defaults: defaults), .comfortable)

        PanelDensityPreference.setAccountCount(nil)
        XCTAssertEqual(PanelDensityPreference.resolved(defaults: defaults), .comfortable)
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
