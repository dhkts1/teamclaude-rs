import XCTest

@testable import TcrBarCore

/// "Keep this Mac awake" surviving a restart, and the four ways that can be
/// wrong without anything looking broken.
///
/// A scratch suite per test, the same way `LaunchPreferenceTests` does it: these
/// assert on stored preferences, and a suite that wrote to the operator's real
/// domain would arm or disarm their actual setting from a test run.
@MainActor
final class AwakePersistenceTests: XCTestCase {
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

    /// The literal is load-bearing: renaming it fails no test and no build, and
    /// the first symptom is a Mac that went to sleep during a long run.
    func testTheKeyIsTheStringTheStoredPreferenceUses() {
        XCTAssertEqual(AwakeController.keepAwakeKey, "keepThisMacAwake")
    }

    func testTickingItArmsTheNextLaunch() {
        let fake = RecordingActivity()
        let controller = AwakeController(activity: fake.activity, defaults: defaults)

        controller.setOn(true)

        XCTAssertTrue(defaults.bool(forKey: AwakeController.keepAwakeKey))

        // The next launch: a fresh controller over the same stored intent.
        let next = RecordingActivity()
        let relaunched = AwakeController(activity: next.activity, defaults: defaults)
        XCTAssertFalse(relaunched.isOn, "nothing is held until the restore runs")
        relaunched.restoreFromPreference()
        XCTAssertTrue(relaunched.isOn)
        XCTAssertEqual(next.begun.count, 1, "restored by taking the assertions exactly once")
    }

    func testUntickingItDisarmsTheNextLaunch() {
        let fake = RecordingActivity()
        let controller = AwakeController(activity: fake.activity, defaults: defaults)

        controller.setOn(true)
        controller.setOn(false)

        XCTAssertFalse(defaults.bool(forKey: AwakeController.keepAwakeKey))

        let next = RecordingActivity()
        let relaunched = AwakeController(activity: next.activity, defaults: defaults)
        relaunched.restoreFromPreference()
        XCTAssertFalse(relaunched.isOn)
        XCTAssertEqual(next.begun.count, 0, "an unticked box takes nothing at launch")
    }

    /// The trap this whole feature dies on, silently.
    ///
    /// `releaseOnQuit()` ends the assertions on the way out. Had the stored
    /// value been written from the token's own state rather than from what
    /// ``AwakeController/setOn(_:)`` was asked for, quitting would have recorded
    /// OFF — every single time — and the setting would have been dead on
    /// arrival while every other test in this file still passed.
    func testQuittingReleasesTheAssertionsWithoutUnaskingForThem() {
        let fake = RecordingActivity()
        let controller = AwakeController(activity: fake.activity, defaults: defaults)

        controller.setOn(true)
        controller.releaseOnQuit()

        XCTAssertFalse(controller.isOn, "the assertions are released")
        XCTAssertEqual(fake.live.count, 0)
        XCTAssertTrue(
            defaults.bool(forKey: AwakeController.keepAwakeKey),
            "quitting ends the hold; it does not un-ask for it")

        let next = RecordingActivity()
        let relaunched = AwakeController(activity: next.activity, defaults: defaults)
        relaunched.restoreFromPreference()
        XCTAssertTrue(relaunched.isOn, "so the next launch comes back holding")
    }

    /// A take that fails leaves the panel honestly OFF — and leaves the intent
    /// stored, so the next launch tries again instead of quietly disabling a
    /// setting the operator never turned off.
    func testAFailedTakeReportsOffAndStillArmsTheNextLaunch() {
        let refusing = AwakeController.Activity(begin: { _ in nil }, end: { _ in })
        let controller = AwakeController(activity: refusing, defaults: defaults)

        controller.setOn(true)

        XCTAssertFalse(controller.isOn, "nothing is held, so the control says so")
        XCTAssertTrue(defaults.bool(forKey: AwakeController.keepAwakeKey))
    }

    /// The harness pairing. Rendering the ON scene must not reach the operator's
    /// preferences — a PNG run would otherwise write `true` on scene 12 and
    /// `false` on the next one, disarming a setting they had turned on.
    func testTheHarnessControllerRemembersNothing() {
        defaults.set(true, forKey: AwakeController.keepAwakeKey)

        let harness = AwakeController.harness()
        harness.setOn(true)
        harness.setOn(false)

        XCTAssertTrue(
            defaults.bool(forKey: AwakeController.keepAwakeKey),
            "the harness drove the control and wrote nothing")
        XCTAssertFalse(
            UserDefaults.standard.bool(forKey: AwakeController.keepAwakeKey),
            "and it did not reach the standard domain either")

        harness.restoreFromPreference()
        XCTAssertFalse(harness.isOn, "a controller that remembers nothing restores nothing")
    }
}
