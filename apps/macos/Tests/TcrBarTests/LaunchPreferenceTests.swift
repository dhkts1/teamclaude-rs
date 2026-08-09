import XCTest

@testable import TcrBarCore

/// "Start server at launch" moved out of `@AppStorage` when the SwiftUI `App`
/// struct went away. The value is the operator's, and it is already set on at
/// least one machine, so the thing worth testing is that nothing about how it is
/// stored changed.
@MainActor
final class LaunchPreferenceTests: XCTestCase {

    private var suiteName = ""
    private var defaults = UserDefaults.standard

    override func setUp() {
        super.setUp()
        // A scratch suite, never `.standard`: a test that writes the real key
        // would silently change whether the operator's proxy comes up at login.
        suiteName = "tcrbar.tests.\(UUID().uuidString)"
        defaults = UserDefaults(suiteName: suiteName) ?? .standard
    }

    override func tearDown() {
        UserDefaults.standard.removePersistentDomain(forName: suiteName)
        super.tearDown()
    }

    /// The literal, pinned.
    ///
    /// Renaming it fails nothing and breaks something: the stored value is
    /// orphaned, the preference silently reverts to off, and the first symptom is
    /// a proxy that stopped coming up at login — a long way from the cause.
    func testTheDefaultsKeyIsTheOneAppStorageUsed() {
        XCTAssertEqual(LaunchPreference.startServerAtLaunchKey, "startServerAtLaunch")
    }

    /// Same default as the `@AppStorage` declaration carried. Opt-in on purpose:
    /// once TcrBar supervises the server, quitting TcrBar stops it.
    func testAnAbsentKeyReadsAsOff() {
        XCTAssertFalse(LaunchPreference(defaults: defaults).startServerAtLaunch)
    }

    func testAStoredValueIsRead() {
        defaults.set(true, forKey: LaunchPreference.startServerAtLaunchKey)
        XCTAssertTrue(LaunchPreference(defaults: defaults).startServerAtLaunch)
    }

    /// Written through immediately, not on quit. The panel and the next launch
    /// read the same fact, so they cannot disagree about it.
    func testSettingItWritesThroughAndSurvivesANewInstance() {
        let preference = LaunchPreference(defaults: defaults)
        preference.startServerAtLaunch = true

        XCTAssertTrue(defaults.bool(forKey: LaunchPreference.startServerAtLaunchKey))
        XCTAssertTrue(LaunchPreference(defaults: defaults).startServerAtLaunch)

        preference.startServerAtLaunch = false
        XCTAssertFalse(defaults.bool(forKey: LaunchPreference.startServerAtLaunchKey))
        XCTAssertFalse(LaunchPreference(defaults: defaults).startServerAtLaunch)
    }

    /// Reading the stored value in `init` must not write it back — a property
    /// initialised in `init` does not fire `didSet`, and this is the test that
    /// says so out loud.
    func testConstructingItDoesNotWriteTheKey() {
        _ = LaunchPreference(defaults: defaults)
        XCTAssertNil(defaults.object(forKey: LaunchPreference.startServerAtLaunchKey))
    }
}
