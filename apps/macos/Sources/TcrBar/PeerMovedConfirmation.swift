import AppKit
import TcrBarCore

/// The ask, and the two runs around it, for a clicked `tcr://peer/moved` link.
///
/// `NSAlert`, the same shape ``PeerJoinConfirmation`` uses and for the same
/// reason: a link arrives by being clicked in a chat app, which LAUNCHES this
/// app, so there may be no panel and no window on screen. An app-modal alert
/// is the one surface that works with nothing else open.
///
/// # Read first, keep second, and the person in between
///
/// `tcr peer moved open` splits in two: without `--yes` it reads the link and
/// writes nothing, with `--yes` it keeps what the first run showed. This type
/// runs both halves around one alert, so what a person agrees to is the
/// CLI's own account of what would change, printed before anything did.
///
/// The words on screen are `tcr`'s, verbatim. This app does not summarise
/// them, shorten them or decide what they mean: the CLI is the one thing that
/// opens the record, and a second voice describing it would be a second
/// answer to give when the two disagree.
///
/// A link `tcr` refused stops here with that sentence and nothing else. There
/// is no button on that alert that keeps, pairs or trusts anything. A link
/// forwarded into the wrong chat would otherwise turn into a trust prompt in
/// front of someone the sender never meant to ask, which is the state
/// ``PeerJoinConfirmation`` was written to end for the other link.
enum PeerMovedConfirmation {
    /// Read the link, ask, and keep the addresses only if the answer is yes.
    ///
    /// Blocking on the subprocess and `nonisolated`, so the URL handler can
    /// call it off the main actor: this runs `tcr` twice and the app must not
    /// stall on either. The answer is one sentence for the log, never the
    /// link, because the string that arrived can be replayed by anyone holding
    /// it for as long as it stays good.
    nonisolated static func readAskKeep(url: URL) async -> String {
        let preview: PeerSecretInvocation
        switch PeerCommand.moved(open: url) {
        case .failure(let refusal):
            return PeerMovedLink.sentence(for: refusal)
        case .success(let invocation):
            preview = invocation
        }
        let read: PeerMovedLink.Answer
        switch run(preview) {
        case .couldNotRun(let why):
            await show(stopped: why)
            return why
        case .answered(let answer):
            read = answer
        }
        guard read.isClean else {
            await show(stopped: read.lines)
            return "refused at the read; nothing was kept"
        }
        guard await ask(keep: read.lines) else {
            return "cancelled at the confirmation alert; nothing was kept"
        }

        guard case .success(let apply) = PeerCommand.moved(open: url, apply: true) else {
            // Unreachable by construction: the same URL built the preview
            // invocation a moment ago. Said out loud rather than forced, so a
            // future change to the shape rules cannot crash this app from a
            // clicked link.
            return "the link stopped being readable between the two runs; nothing was kept"
        }
        switch run(apply) {
        case .couldNotRun(let why):
            await show(stopped: why)
            return why
        case .answered(let answer):
            guard answer.isClean else {
                // The person pressed Keep and it did not happen, so this one
                // is shown rather than only logged. A press that silently did
                // nothing is the shape every confirmation here exists to end.
                await show(stopped: answer.lines)
                return "the keep run failed; nothing was kept"
            }
            return "kept"
        }
    }

    /// The ask itself. `true` only on Keep; a dismiss, an Escape and Cancel
    /// all read as `false`.
    @MainActor
    static func ask(keep lines: String) -> Bool {
        let alert = NSAlert()
        alert.alertStyle = .informational
        alert.messageText = "Keep these addresses?"
        alert.informativeText = lines
        let keep = alert.addButton(withTitle: "Keep")
        alert.addButton(withTitle: "Cancel")
        // Return defaults to Cancel, the same reversal ``QuitConfirmation``
        // and ``PeerJoinConfirmation`` both make: a write this Mac was asked
        // about is not the button a stray Return key should land on.
        keep.keyEquivalent = ""
        alert.buttons.last?.keyEquivalent = "\r"
        return alert.runModal() == .alertFirstButtonReturn
    }

    /// A link that went no further, in the words of whatever stopped it. One
    /// button, and it dismisses.
    @MainActor
    static func show(stopped lines: String) {
        let alert = NSAlert()
        alert.alertStyle = .informational
        alert.messageText = "Nothing was added"
        alert.informativeText = lines
        alert.addButton(withTitle: "OK")
        _ = alert.runModal()
    }

    /// What one run of `tcr peer moved open` answered.
    private enum Run {
        case answered(PeerMovedLink.Answer)
        /// `tcr` was not found, or the process itself failed. Not the CLI's
        /// refusal and not written as one.
        case couldNotRun(String)
    }

    /// Run one half and classify it.
    ///
    /// This does not go through ``PeerController``'s runner, and the reason is
    /// the printed lines: that path reports an exit code and drops stdout,
    /// while the whole point of the read run is showing a person exactly what
    /// `tcr` printed. The link still rides stdin, which is the rule that
    /// mattered.
    private nonisolated static func run(_ invocation: PeerSecretInvocation) -> Run {
        switch TcrTool.resolve() {
        case .failure(let notFound):
            return .couldNotRun(
                "tcr not found (searched \(notFound.searched.count) locations). "
                    + TcrTool.overrideRemedy)
        case .success(let executable):
            do {
                let output = try TcrTool.run(
                    executable: executable, arguments: invocation.arguments,
                    stdin: invocation.stdin)
                return .answered(
                    PeerMovedLink.answer(
                        exitCode: output.exitCode,
                        stdout: String(data: output.stdout, encoding: .utf8) ?? "",
                        stderr: output.stderr))
            } catch {
                return .couldNotRun(error.localizedDescription)
            }
        }
    }
}
