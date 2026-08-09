import Foundation

/// `TcrBar --keep-awake-probe <seconds>` — hold the keep-awake assertion for a
/// fixed interval, from a shell, with no GUI.
///
/// ## Why this exists
///
/// The keep-awake control is a checkbox, and proving it does what `caffeinate`
/// does means reading `pmset -g assertions` while it is on. Nothing on this
/// machine can tick a checkbox for you, and "look at the menu bar" is not
/// available as a gate either: `screencapture` needs Screen Recording, which a
/// headless agent or a build machine may not have been granted.
///
/// So this flag is the oracle. It drives the same ``AwakeController`` the panel
/// drives, prints its own pid, and holds for a stated number of seconds:
///
///     TcrBar --keep-awake-probe 10 &
///     sleep 3
///     pmset -g assertions | grep TcrBar     # PreventUserIdleSystemSleep
///
/// Like `--render-states` and `--render-icon` it is handled in
/// `TcrBarEntry.main()` and exits before the shell is built, so no menu-bar item
/// appears and no `tcr` subprocess is spawned. Unlike those two it draws
/// nothing, so it does not even need an `NSApplication`.
public enum KeepAwakeProbe {
    public static let flag = "--keep-awake-probe"

    /// How long the process stays alive *after* releasing the activity.
    ///
    /// Without this window the gate cannot attribute the release. A power
    /// assertion also disappears when its process dies, so a `pmset` reading
    /// taken after the probe exits is equally consistent with "`endActivity`
    /// released it" and with "it was never released and the kernel cleaned up" —
    /// the check would pass either way, which makes it not a check.
    ///
    /// Sampling inside this window separates them, and that separation was
    /// measured with a scratch build of exactly this shape: at t=3 one `pmset`
    /// line named the pid; at t=10, with the release done and `ps` confirming
    /// the process still running, zero.
    public static let lingerAfterRelease: TimeInterval = 3

    /// What the command line asked for. `nil` from ``request(_:)`` means the
    /// flag is absent and the app should start normally.
    public enum Request: Equatable {
        case hold(seconds: Double)
        /// The flag was given but its argument was not usable. Kept as a case
        /// rather than folded into `nil` because those are opposite outcomes: a
        /// missing flag means "start the app", a broken one means "the operator
        /// meant to probe and got it wrong", and starting a menu-bar app in
        /// answer to a typo hides the mistake behind an icon.
        case usage(problem: String)
    }

    public static func request(_ arguments: [String] = CommandLine.arguments) -> Request? {
        guard let i = arguments.firstIndex(of: flag) else { return nil }
        guard i + 1 < arguments.count else {
            return .usage(problem: "\(flag) needs a duration in seconds")
        }
        let raw = arguments[i + 1]
        // `isFinite` is load-bearing, not defensive noise: `Double("inf")` and
        // `Double("1e400")` both parse, and either would ask `Thread.sleep` to
        // wait forever while holding the assertion — a probe that never releases
        // is the exact failure this whole file exists to detect in the app.
        guard let seconds = Double(raw), seconds.isFinite, seconds > 0 else {
            return .usage(problem: "not a positive number of seconds: '\(raw)'")
        }
        return .hold(seconds: seconds)
    }

    @MainActor
    public static func run(_ request: Request) -> Never {
        switch request {
        case .usage(let problem):
            write(
                to: FileHandle.standardError,
                """
                TcrBar: \(problem)

                usage: TcrBar \(flag) <seconds>

                Holds an idle-system-sleep assertion for <seconds>, releases it,
                then lingers \(Int(lingerAfterRelease))s so the release is observable while this
                process is still alive. Prove it with:

                    pmset -g assertions | grep TcrBar

                """)
            exit(2)

        case .hold(let seconds):
            let controller = AwakeController()
            controller.setOn(true)

            // There is deliberately no `guard controller.isOn` here, and its
            // absence is the honest shape.
            //
            // `beginActivity` returns a non-optional, so `begin()` always
            // stores a token and `isOn` is always true on the next line. A
            // guard there cannot fail: it would read as a check on whether the
            // assertion was really taken while checking nothing at all, which
            // is worse than no check, because the next reader trusts it. What
            // it purported to catch — a probe reporting success while holding
            // nothing — is not observable in this process at all; only `pmset`
            // can see a power assertion, and that reading is the gate in
            // `apps/macos/README.md`.
            let pid = ProcessInfo.processInfo.processIdentifier
            write(
                to: FileHandle.standardOutput,
                """
                holding  pid \(pid)  for \(seconds)s
                assertion name: \(AwakeController.reason)
                check it:  pmset -g assertions | grep 'pid \(pid)'

                """)
            Thread.sleep(forTimeInterval: seconds)

            controller.setOn(false)
            write(
                to: FileHandle.standardOutput,
                """
                released pid \(pid)  (staying alive \(Int(lingerAfterRelease))s so the release is \
                attributable to endActivity, not to this process exiting)

                """)
            Thread.sleep(forTimeInterval: lingerAfterRelease)
            exit(0)
        }
    }

    /// Writes and flushes.
    ///
    /// `print` to a pipe is block-buffered, and this probe is meant to be run
    /// with `&` and its output captured — so the "holding" line would not appear
    /// until the process exited, by which point it says nothing useful.
    private static func write(to handle: FileHandle, _ text: String) {
        handle.write(Data(text.utf8))
    }
}
