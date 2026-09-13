import AppKit

/// The one confirm-before-quit alert, shared by Settings' "Quit TcrBar…" button
/// and the panel footer's "Quit" button.
///
/// Before this type existed the two callers each built their own `NSAlert` —
/// same title, same cost sentence, copied by hand — which is exactly the shape
/// `CLAUDE.md`'s "no silent fallbacks" sibling rule warns about for any fact
/// that lives in two places: the day one wording changes and the other does
/// not, an operator sees a different warning depending on which button they
/// clicked. One function, called from both places, is the whole fix.
enum QuitConfirmation {
    /// Ask, then terminate the app if the operator confirms.
    ///
    /// The cost sentence matches `CLAUDE.md`'s own: quitting stops the proxy
    /// this app supervises, so every live session loses its prompt cache —
    /// the most expensive event in this system.
    static func confirm() {
        let alert = NSAlert()
        alert.alertStyle = .critical
        alert.messageText = "Quit TcrBar?"
        alert.informativeText =
            "This stops the proxy TcrBar supervises. Every live session loses its "
            + "prompt cache."
        let quit = alert.addButton(withTitle: "Quit")
        quit.hasDestructiveAction = true
        alert.addButton(withTitle: "Cancel")
        quit.keyEquivalent = ""
        alert.buttons.last?.keyEquivalent = "\r"
        guard alert.runModal() == .alertFirstButtonReturn else { return }
        NSApplication.shared.terminate(nil)
    }
}
