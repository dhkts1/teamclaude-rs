import XCTest

@testable import TcrBarCore

/// Regression test for an app that could not be launched.
///
/// TcrBar's whole UI is one `MenuBarExtra`. Hiding the status item tore that scene
/// down, a SwiftUI `App` with zero scenes terminates itself, and AppKit's default
/// `applicationShouldTerminate` answer — YES — meant the app agreed. Every launch
/// exited 0 within seconds with no output and no crash report, so it read as a
/// broken build rather than a quit. That cost a day of no menu-bar app.
///
/// The fix inverts the default: refuse, unless something identifiable asked. The
/// danger in that inversion is the opposite bug — an app that cannot be quit — so
/// what these tests actually guard is the ACCEPT list. Each case below is a way
/// the app legitimately has to be able to exit, and dropping any one of them
/// ships something worse than the failure being fixed.
final class TerminationPolicyTests: XCTestCase {

    /// The scene teardown itself: an in-process `terminate:` with no Apple event
    /// and nothing having authorized it. The one case that must be refused.
    func testAnUnrequestedTerminationIsRefused() {
        let policy = TerminationPolicy()
        XCTAssertNil(policy.authorization)
        XCTAssertFalse(policy.allowsTermination(externalQuitRequest: false))
    }

    /// The panel's Quit button. It authorizes and then terminates.
    func testTheQuitButtonIsAllowed() {
        let policy = TerminationPolicy()
        policy.authorize(.userChoseQuit)
        XCTAssertTrue(policy.allowsTermination(externalQuitRequest: false))
        XCTAssertEqual(policy.authorization, .userChoseQuit)
    }

    /// A logout, restart or shutdown. An accessory app that refuses these blocks
    /// the whole power-off and gets blamed for it.
    func testPowerOffIsAllowed() {
        let policy = TerminationPolicy()
        policy.authorize(.systemIsPoweringOff)
        XCTAssertTrue(policy.allowsTermination(externalQuitRequest: false))
    }

    /// Sparkle terminating to install and relaunch. Refused, updates would
    /// silently never install.
    func testASparkleRelaunchIsAllowed() {
        let policy = TerminationPolicy()
        policy.authorize(.updateWillRelaunch)
        XCTAssertTrue(policy.allowsTermination(externalQuitRequest: false))
    }

    /// `osascript -e 'quit app id "…"'` and a logout's quit both arrive as a
    /// `kAEQuitApplication` Apple event, which the app handles itself and
    /// authorizes before terminating.
    ///
    /// This case is the one that was measured BROKEN. The first version relied on
    /// reading `NSAppleEventManager.currentAppleEvent` inside
    /// `applicationShouldTerminate`; it returned no event, the refusal stood, and
    /// the quit timed out after 25 s with the app still running. Authorizing from
    /// the app's own quit handler does not depend on that timing.
    func testAQuitAppleEventIsAllowed() {
        let policy = TerminationPolicy()
        policy.authorize(.quitEventReceived)
        XCTAssertTrue(policy.allowsTermination(externalQuitRequest: false))
    }

    /// The belt that survived that measurement: if AppKit ever does report the
    /// quit event as current, that alone is enough, with no prior authorization.
    func testAnExternalQuitRequestIsAllowedWithoutPriorAuthorization() {
        let policy = TerminationPolicy()
        XCTAssertNil(policy.authorization)
        XCTAssertTrue(policy.allowsTermination(externalQuitRequest: true))
    }

    /// First writer wins, and it is never revoked. A Quit pressed while Sparkle is
    /// mid-relaunch must not be able to turn an authorized termination back into a
    /// refusal and wedge the app open.
    func testAuthorizationIsNeverRevokedOrOverwritten() {
        let policy = TerminationPolicy()
        policy.authorize(.updateWillRelaunch)
        policy.authorize(.userChoseQuit)
        XCTAssertEqual(policy.authorization, .updateWillRelaunch)
        XCTAssertTrue(policy.allowsTermination(externalQuitRequest: false))
    }

    /// Sparkle calls its delegate off the main thread and terminates immediately
    /// afterwards, which is why this type is lock-guarded rather than
    /// `@MainActor` — an authorization that lands after the terminate is a
    /// refused update. Concurrent writers must not corrupt or lose the answer.
    func testAuthorizingFromManyThreadsIsSafe() {
        let policy = TerminationPolicy()
        DispatchQueue.concurrentPerform(iterations: 64) { index in
            policy.authorize(index.isMultiple(of: 2) ? .userChoseQuit : .updateWillRelaunch)
        }
        XCTAssertNotNil(policy.authorization)
        XCTAssertTrue(policy.allowsTermination(externalQuitRequest: false))
    }

    /// The shared instance is what the app actually consults; asserting it exists
    /// and starts refusing keeps the singleton from drifting away from the type
    /// the rest of these tests exercise.
    @MainActor
    func testTheSharedPolicyStartsOutRefusing() {
        // Nothing in this test bundle authorizes it, and the app is not running.
        XCTAssertFalse(TerminationPolicy.shared.allowsTermination(externalQuitRequest: false))
    }
}
