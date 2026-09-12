import XCTest

@testable import TcrBarCore

/// "Show counts in the menu bar" (F5, `data/plans/menubar-counts-bridge.md`).
/// Same storage shape as `LaunchPreferenceTests`, with the one deliberate
/// difference this type exists for: the default is ON.
@MainActor
final class MenuBarCountsPreferenceTests: XCTestCase {

    private var suiteName = ""
    private var defaults = UserDefaults.standard

    override func setUp() {
        super.setUp()
        // A scratch suite, never `.standard`: a test that writes the real key
        // would silently flip the operator's own menu-bar preference.
        suiteName = "tcrbar.tests.\(UUID().uuidString)"
        defaults = UserDefaults(suiteName: suiteName) ?? .standard
    }

    override func tearDown() {
        UserDefaults.standard.removePersistentDomain(forName: suiteName)
        super.tearDown()
    }

    /// The literal, pinned — the same trap `LaunchPreference.startServerAtLaunchKey`
    /// documents: renaming it silently resets an operator's choice.
    func testTheDefaultsKeyIsPinned() {
        XCTAssertEqual(MenuBarCountsPreference.showCountsKey, "showCountsInMenuBar")
    }

    /// The one place this type disagrees with `LaunchPreference`: an absent
    /// key reads as ON, not off. `UserDefaults.bool(forKey:)` alone would say
    /// `false` here, which is exactly the wrong default for a feature meant to
    /// be visible from the first launch that carries it.
    func testAnAbsentKeyReadsAsOn() {
        XCTAssertNil(defaults.object(forKey: MenuBarCountsPreference.showCountsKey))
        XCTAssertTrue(MenuBarCountsPreference(defaults: defaults).showCounts)
    }

    func testAStoredFalseValueIsRead() {
        defaults.set(false, forKey: MenuBarCountsPreference.showCountsKey)
        XCTAssertFalse(MenuBarCountsPreference(defaults: defaults).showCounts)
    }

    func testAStoredTrueValueIsRead() {
        defaults.set(true, forKey: MenuBarCountsPreference.showCountsKey)
        XCTAssertTrue(MenuBarCountsPreference(defaults: defaults).showCounts)
    }

    /// Written through immediately, not on quit — the toggle and the next
    /// launch read the same fact, so they cannot disagree about it.
    func testSettingItWritesThroughAndSurvivesANewInstance() {
        let preference = MenuBarCountsPreference(defaults: defaults)
        preference.showCounts = false

        XCTAssertFalse(defaults.bool(forKey: MenuBarCountsPreference.showCountsKey))
        XCTAssertFalse(MenuBarCountsPreference(defaults: defaults).showCounts)

        preference.showCounts = true
        XCTAssertTrue(defaults.bool(forKey: MenuBarCountsPreference.showCountsKey))
        XCTAssertTrue(MenuBarCountsPreference(defaults: defaults).showCounts)
    }

    /// Reading the stored value in `init` must not write it back — a property
    /// initialised in `init` does not fire `didSet`, same as `LaunchPreference`.
    func testConstructingItDoesNotWriteTheKey() {
        _ = MenuBarCountsPreference(defaults: defaults)
        XCTAssertNil(defaults.object(forKey: MenuBarCountsPreference.showCountsKey))
    }
}
