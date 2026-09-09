import Foundation

/// The lines an uncaught `NSException` writes to the unified log on its way out.
///
/// A crash report does not carry the exception's reason string. Three `.ips`
/// files from the 2026-09-09 runaway-layout crash were read end to end and the
/// only human-readable string in any of them is `abort() called`:
/// `lastExceptionBacktrace` gives the throwing frames, and nothing gives the
/// message the exception was constructed with. For an AppKit consistency
/// exception that message IS the diagnosis — it names the offending window and,
/// for the update-constraints guard, the pass count that tripped it. Without it
/// a reader can see that the popover looped but not what it looped on, which is
/// the difference between choosing a fix and guessing at one.
///
/// Pure, and in `TcrBarCore`, so the format is something a test runs rather than
/// something only a crash produces.
public enum UncaughtExceptionReport {
    /// The prefix every line carries, so one `log show --predicate` finds the
    /// whole report and a `grep` finds it in a pasted console dump.
    public static let marker = "TcrBar-uncaught:"

    /// `maxFrames` bounds what a crashing process writes. The handler runs with
    /// the runtime already in an undefined state, so an unbounded walk over a
    /// deep stack is a second way to die inside the first one. 24 is past the
    /// AppKit/SwiftUI cycle in the reports that motivated this and well short of
    /// the 65-frame stacks they carry.
    ///
    /// A nil `reason` renders explicitly rather than as an empty tail: "the
    /// exception carried no message" and "the handler dropped the message" look
    /// identical in a log otherwise, and they call for different fixes.
    public static func lines(
        name: String,
        reason: String?,
        callStack: [String],
        maxFrames: Int = 24
    ) -> [String] {
        let limit = max(0, maxFrames)
        var out = ["\(marker) \(name): \(reason ?? "(no reason)")"]
        for (index, frame) in callStack.prefix(limit).enumerated() {
            out.append("\(marker) \(index) \(frame)")
        }
        if callStack.count > limit {
            out.append("\(marker) … \(callStack.count - limit) more frames")
        }
        return out
    }
}
