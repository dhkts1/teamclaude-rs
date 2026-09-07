import Darwin

/// Sends the supervised child a termination signal outside the normal quit
/// path — the one `applicationWillTerminate` (`TcrBarApp.swift`) never runs,
/// because AppKit does not call app-delegate methods when the process itself
/// is killed by a signal.
///
/// `TcrBarApp.swift` installs a C signal handler for `SIGABRT` and the other
/// signals a native crash raises, and that handler's entire body is one call
/// into ``terminateSupervisedChild(pid:)``. A signal handler runs on
/// whatever thread raised the signal, with malloc and the rest of the Swift
/// runtime's usual guarantees suspended — `kill(2)` is on POSIX's
/// async-signal-safe list, essentially nothing else is, so this is the whole
/// of what the handler may do. Pulling it out as a free function, rather than
/// inlining it in the handler, is what lets a test call it directly without
/// raising a real signal.
///
/// Without this, a `SIGABRT` — an uncaught `NSException` converts to exactly
/// that — orphans the child `tcr server` process, which keeps holding
/// port 3456. TcrBar's own next launch then correctly stands down rather
/// than fight an incumbent it does not recognise, and shows
/// `.incumbentHoldsPort`.
public enum AbnormalTerminationGuard {
    /// `pid <= 0` is never a supervised child — `0` is
    /// ``ServerController/supervisedChildPID``'s "nothing is supervised"
    /// value, and a negative pid means "every process in the group" to
    /// `kill(2)`, which this must never send.
    public static func terminateSupervisedChild(pid: pid_t) {
        guard pid > 0 else { return }
        kill(pid, SIGTERM)
    }
}
