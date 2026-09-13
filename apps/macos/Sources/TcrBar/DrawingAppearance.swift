import AppKit

/// Runs `body` with `appearance` installed as the drawing appearance, then puts
/// the previous one back.
///
/// The macOS 12 replacement for assigning `NSAppearance.current`, which is
/// deprecated and has no replacement setter: `performAsCurrentDrawingAppearance`
/// is now the only supported way to make a dynamic `NSColor` resolve against a
/// chosen appearance rather than the process's own. Every colour token in this
/// app resolves through such a provider, so a render harness that skips this
/// writes the light palette into a file named `-dark`.
///
/// A `nil` appearance — a name AppKit did not recognise — runs `body` under
/// whatever appearance is already current, which is what the old assignment did
/// as well: `NSAppearance.current = nil` cleared the override rather than
/// failing.
@MainActor
func withDrawingAppearance<Result>(
    _ appearance: NSAppearance?, perform body: () -> Result
) -> Result {
    guard let appearance else { return body() }
    var result: Result?
    appearance.performAsCurrentDrawingAppearance { result = body() }
    guard let result else {
        // Unreachable: the block is non-escaping and runs synchronously, before
        // `performAsCurrentDrawingAppearance` returns. Stated rather than
        // force-unwrapped so a future AppKit that stops calling it fails loudly
        // instead of writing a half-rendered PNG.
        preconditionFailure("performAsCurrentDrawingAppearance did not run its block")
    }
    return result
}
