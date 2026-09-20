import Foundation

/// Whether the peers read is allowed to run right now, asked once per tick.
///
/// ## What it is for
///
/// The Peers tab reads `tcr peer ls` and `tcr peer status` every three seconds
/// while it is on screen, started from the view's `onAppear` and stopped from
/// its `onDisappear`. Measured with `--measure-idle 30`, before this existed:
/// the panel was opened on the Peers tab, closed, and ten `peer ls` and ten
/// `peer status` children were spawned in the thirty seconds that followed, the
/// same three second cadence as when it was open. A popover that closes does
/// not necessarily tear its content down, so `onDisappear` is a teardown hook
/// being used as a went-away hook, and there is no teardown.
///
/// ## Why the shell is asked rather than telling
///
/// A flag the panel SETS when it closes would miss the ordinary way this panel
/// goes away: it is `.transient`, so a click outside or Escape dismisses it
/// without anything in this app running a line. Whoever owns the panel
/// registers a closure here instead and it is read at the moment the answer is
/// needed, so every route to a closed panel is covered by one question.
///
/// Unregistered means run, which is what the render harness, the previews and
/// every test are in: no shell, no panel, no claim about one either.
@MainActor
public enum PeerPollGate {
    /// Answered by whoever owns the panel. One writer, set once, and the
    /// reasoning for a registered closure rather than an injected dependency is
    /// that the controller is built by a view the panel's owner does not
    /// construct.
    public static var panelIsOnScreen: (() -> Bool)?

    /// `true` when a read may run: the panel is on screen, or nobody is
    /// claiming to know.
    public static func shouldRead() -> Bool {
        panelIsOnScreen?() ?? true
    }
}
