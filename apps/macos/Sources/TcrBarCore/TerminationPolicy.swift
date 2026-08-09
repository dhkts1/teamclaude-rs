import Foundation

/// Whether this process is allowed to exit.
///
/// # The bug this exists to prevent
///
/// TcrBar's entire UI is one `MenuBarExtra`, so `TcrBarApp.body` declares exactly
/// one scene. When Control Center reports the status item hidden, SwiftUI tears
/// that scene down — and a SwiftUI `App` left with zero scenes terminates itself.
/// Traced live on 2026-08-09 from a real launch:
///
///     FrontBoard  Request for <FBSScene …-NSStatusItemView> complete!
///     FrontBoard  Received action(s): NSStatusItemChangeVisibilityAction
///     AppKit:Application  terminate:                        <- 1 ms later
///     AppKit:Application  replyToApplicationShouldTerminate:YES
///
/// The app was not crashing. It was *agreeing to quit*: `AppDelegate` implemented
/// no `applicationShouldTerminate(_:)`, so AppKit's default `YES` is what that
/// last line records. Every launch exited 0 within 0.1–12 s with empty output and
/// no crash report, which is why it read as a launch failure rather than a quit.
/// A hidden icon therefore made the app unlaunchable for a whole day.
///
/// # The rule
///
/// Termination is REFUSED by default and allowed only when something identifiable
/// asked for it. That inverts AppKit's default, so the interesting part is the
/// list of things that legitimately ask — miss one and the cure is worse than the
/// disease, because an app that cannot be quit is a worse bug than one that quits
/// too easily. There are two ways in:
///
///  * `authorize(_:)`, called by the code that is *about* to terminate the app —
///    the panel's Quit button, the power-off notification (a logout or shutdown
///    must not be blocked by an accessory app), and Sparkle immediately before it
///    relaunches into a new version.
///  * `externalQuitRequest`, passed by `AppDelegate` when the terminate arrived
///    as a `kAEQuitApplication` Apple event. That covers every quit request from
///    *outside* the process — `osascript -e 'quit app id "…"'`, the Dock, Cmd-Q
///    routed through the standard menu — none of which run any of our code first
///    and so cannot have called `authorize(_:)`.
///
/// The scene teardown above matches neither: it is an in-process `terminate:` with
/// no Apple event behind it, which is exactly the signal that separates it from
/// every deliberate quit.
///
/// # Thread safety
///
/// Deliberately lock-guarded rather than `@MainActor`. Sparkle calls
/// `updaterWillRelaunchApplication` and then terminates; hopping to the main actor
/// to record the authorization would race that termination and could lose it, and
/// a lost authorization here means a refused update. A lock makes the write land
/// before the call returns, whichever thread it arrives on.
public final class TerminationPolicy: @unchecked Sendable {
    /// Who asked. Recorded rather than reduced to a flag so the log line names the
    /// reason — "TcrBar refused to quit" is only debuggable if the accepted cases
    /// are equally legible.
    public enum Authorization: String, Sendable {
        /// The Quit button in the panel.
        case userChoseQuit
        /// `NSWorkspace.willPowerOffNotification` — a logout, restart or shutdown.
        case systemIsPoweringOff
        /// Sparkle is about to install an update and relaunch.
        case updateWillRelaunch
    }

    /// One process, one answer to "may I exit". A singleton because the question
    /// is genuinely process-global: the Quit button, the delegate and Sparkle's
    /// delegate all have to agree, and none of them can see the others' state.
    public static let shared = TerminationPolicy()

    /// Public so tests can exercise the policy without touching process state.
    public init() {}

    private let lock = NSLock()
    private var storedAuthorization: Authorization?

    /// The reason termination was authorized, or `nil` if it never was.
    public var authorization: Authorization? {
        lock.lock()
        defer { lock.unlock() }
        return storedAuthorization
    }

    /// Record that a termination is expected.
    ///
    /// First writer wins. The reasons are not ranked and it is never revoked: once
    /// something legitimate has asked the app to go away, a later caller must not
    /// be able to turn that back into a refusal, or a Quit pressed during a
    /// Sparkle relaunch could wedge the app open.
    public func authorize(_ reason: Authorization) {
        lock.lock()
        defer { lock.unlock() }
        if storedAuthorization == nil {
            storedAuthorization = reason
        }
    }

    /// The decision itself, kept pure so it can be tested without an `NSApplication`.
    ///
    /// - Parameter externalQuitRequest: true when the terminate currently being
    ///   handled arrived as a `kAEQuitApplication` Apple event.
    public func allowsTermination(externalQuitRequest: Bool) -> Bool {
        externalQuitRequest || authorization != nil
    }
}
