import AppKit
import TcrBarCore

/// The one confirm-before-join alert for a clicked `tcr://peer/join` link.
///
/// Before this type existed, `AppDelegate.handleJoinLink` piped the whole
/// link straight to `tcr peer join --stdin` the instant the URL arrived: one
/// clicked link, and this Mac was re-keyed onto whatever mesh the link named,
/// with nothing on screen to say so. Trust (the peer-pairing flow) never
/// moves a credential without the operator first comparing six digits on both
/// ends; a link had never asked to be looked at, even once.
///
/// `NSAlert`, the same shape ``QuitConfirmation`` uses, because a link can
/// arrive before any panel or window exists: opening one in a chat app
/// LAUNCHES this app, and an app-modal alert is the one UI surface that
/// works with no other window open.
enum PeerJoinConfirmation {
    /// Ask, on the main thread. `true` only on the destructive button; a
    /// dismiss, an Escape, or Cancel all read as `false`.
    ///
    /// `hasExistingKey` only changes the WORDING (``PeerJoinLink/confirmationBody(for:hasExistingKey:)``)
    /// and whether Join is styled destructive; `tcr peer join` itself is
    /// still the one place that actually refuses an overwrite.
    @MainActor
    static func confirm(url: URL, hasExistingKey: Bool) -> Bool {
        let alert = NSAlert()
        alert.alertStyle = hasExistingKey ? .critical : .warning
        alert.messageText = "Join this mesh?"
        alert.informativeText = PeerJoinLink.confirmationBody(for: url, hasExistingKey: hasExistingKey)
        let join = alert.addButton(withTitle: "Join")
        join.hasDestructiveAction = hasExistingKey
        alert.addButton(withTitle: "Cancel")
        // Return defaults to Cancel, the same reversal `QuitConfirmation`
        // makes: a credential change is not the button a stray Return key
        // should ever land on.
        join.keyEquivalent = ""
        alert.buttons.last?.keyEquivalent = "\r"
        return alert.runModal() == .alertFirstButtonReturn
    }
}
