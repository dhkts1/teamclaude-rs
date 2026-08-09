import Combine
import Foundation

/// "Keep this Mac awake" — what `caffeinate -i` does, held for exactly as long
/// as the control is on.
///
/// ## Mechanism, measured rather than assumed
///
/// `ProcessInfo.beginActivity(options:reason:)` with `.idleSystemSleepDisabled`
/// (`NSActivityIdleSystemSleepDisabled`, "the computer must not idle sleep").
/// With the activity held, `pmset -g assertions` lists the process by pid under
/// `PreventUserIdleSystemSleep`, named with ``reason`` — the same assertion type
/// `caffeinate` registers. ``KeepAwakeProbe`` is the flag that produces that
/// line from a shell, and it is the gate: this claim is checkable in about
/// fifteen seconds and should be re-checked rather than believed.
///
/// ## Deliberate scope
///
/// **Idle *system* sleep only, never display sleep.** The job is "a long run
/// keeps running", and the screen going dark does not stop a run. Holding the
/// display awake all night is a real cost — backlight, burn-in, a bright room —
/// that nobody asked for. `.idleDisplaySleepDisabled` exists and is not used.
///
/// **This does not survive closing the lid.** No assertion of this class does;
/// a clamshell sleep still sleeps. The panel says so, because an operator who
/// believes otherwise comes back to a dead run and blames the proxy.
///
/// **Not persisted across launches.** There is deliberately no `@AppStorage`
/// here. A Mac that silently never sleeps because of a box ticked a week ago is
/// a worse bug than having to tick it again — the symptom (a laptop that runs
/// hot in a bag) is nowhere near the cause. Contrast `startServerAtLaunch`,
/// which *is* persisted: being wrong about that one costs a spawn that stands
/// down harmlessly.
@MainActor
public final class AwakeController: ObservableObject {

    /// The begin/end pair, injected.
    ///
    /// Nothing in-process can observe another process's power assertions, so a
    /// test against the real `ProcessInfo` could only prove that the calls did
    /// not throw — while leaving the machine awake for as long as the suite ran.
    /// Taking the pair as a dependency is what makes the property that actually
    /// matters testable: that begin and end are called exactly once each, on the
    /// same token. `LoginItem.classify` is the precedent — keep the part with a
    /// decision in it separable from the part that talks to the system.
    public struct Activity {
        public let begin: (String) -> NSObjectProtocol
        public let end: (NSObjectProtocol) -> Void

        public init(
            begin: @escaping (String) -> NSObjectProtocol,
            end: @escaping (NSObjectProtocol) -> Void
        ) {
            self.begin = begin
            self.end = end
        }

        /// The real one.
        public static let processInfo = Activity(
            begin: { reason in
                ProcessInfo.processInfo.beginActivity(
                    options: .idleSystemSleepDisabled, reason: reason)
            },
            end: { ProcessInfo.processInfo.endActivity($0) }
        )

        /// A pair that holds nothing.
        ///
        /// For the PNG harness, which has to draw the panel in its ON state and
        /// must not stop the machine sleeping in order to do it. Rendering a
        /// checkbox is not a reason to hold a power assertion.
        public static let inert = Activity(begin: { _ in NSObject() }, end: { _ in })
    }

    /// The assertion's name in `pmset -g assertions`, and therefore the string a
    /// human greps for when their Mac will not sleep and they want to know who
    /// is holding it open.
    ///
    /// Stable and greppable on purpose: it contains "TcrBar", the gate in
    /// `apps/macos/README.md` matches on that, and rewording it silently breaks
    /// both the gate and the human's search.
    public static let reason = "TcrBar is keeping this Mac awake"

    /// Whether the activity is held. Read by the panel and the menu bar.
    @Published public private(set) var isOn = false

    /// The live token — the single source of truth, and the only thing that can
    /// move ``isOn``.
    ///
    /// A separate `Bool` is the defect this shape exists to prevent: any path
    /// that set the flag without moving the token (or the reverse) would leave
    /// the panel reporting a state of the *machine* that is not true. Because
    /// the mirror is maintained here and nowhere else, "the checkbox is ticked"
    /// and "a token is held" cannot disagree.
    private var token: NSObjectProtocol? {
        didSet { isOn = token != nil }
    }

    private let activity: Activity

    public init(activity: Activity = .processInfo) {
        self.activity = activity
    }

    public func setOn(_ on: Bool) {
        if on { begin() } else { end() }
    }

    public func toggle() {
        setOn(!isOn)
    }

    /// Idempotent, and that is the whole point.
    ///
    /// A second `beginActivity` would return a second token, and this class can
    /// only store one — so the first would leak, unreleasable for the lifetime
    /// of the process. The panel would then show OFF while the Mac still refused
    /// to sleep: a control that lies about the state of the machine, with no way
    /// back short of quitting the app.
    private func begin() {
        guard token == nil else { return }
        token = activity.begin(Self.reason)
    }

    /// Ends only a token this class began, and clears it as part of ending it.
    ///
    /// The clear happens *first* so there is no instant in which `token` names
    /// an activity that has already been ended — which is the state from which
    /// a double `endActivity` on the same token becomes possible.
    private func end() {
        guard let held = token else { return }
        token = nil
        activity.end(held)
    }

    /// Called from `applicationWillTerminate`.
    ///
    /// The kernel releases every power assertion a process holds when it dies,
    /// so this is not what lets the Mac sleep again — that would happen anyway.
    /// It is here because "quitting TcrBar releases the assertion" should be
    /// something this app *does*, not something it gets away with. Foundation
    /// makes the same promise from the other side: per `NSProcessInfo.h`, an
    /// activity token deallocated before `endActivity:` ends its activity
    /// automatically.
    public func releaseOnQuit() {
        end()
    }
}
