import Foundation

/// The `tcr://peer/moved` link: one Mac telling one Mac it is already trusted
/// by where it can be reached now, after it changed networks.
///
/// `tcr://peer/moved?v=1&r=<sealed record>`. TcrBar registers the `tcr` scheme
/// in `CFBundleURLTypes` (`apps/macos/scripts/build-tcrbar.sh`), so a link
/// clicked in a chat window opens this app, and the whole string is handed to
/// `tcr peer moved open --stdin`.
///
/// # This is not the share link, and telling them apart is what this type is for
///
/// ``PeerJoinLink`` carries a credential: it can bring an unknown Mac onto the
/// mesh and change what this Mac trusts. This one carries no credential and
/// makes no trust decision. The most it can lead to is a few addresses added
/// to a row this Mac has already pinned, and `tcr` is what holds that bound.
/// One scheme and two paths, so ``route(_:)`` is the single place that decides
/// which of the two arrived; a new path that fell through to the join handler
/// would pipe a moved link into the verb that sets a network key.
///
/// # Two rules inherited from the share link
///
/// **The whole URL goes across, never its parts.** The `r=` field is a sealed
/// record and `tcr` is the only thing that can open it. A panel that pulled it
/// out, decoded it, or pre-judged whether it looks well formed would be a
/// second decider about the same string, free to refuse a link the CLI
/// accepts. So nothing here reads `r=` for anything except whether it is
/// present at all.
///
/// **The record never appears in argv.** argv is readable by every process on
/// this Mac through `ps`, it lands in this process's crash reports, and it is
/// kept in shell history when a person types it. Anyone holding the string can
/// re-apply it for as long as it stays good, so the only thing this type can
/// build is a ``PeerSecretInvocation``, whose argv half is fixed and whose
/// payload rides stdin.
public enum PeerMovedLink {
    /// The scheme, shared with ``PeerJoinLink`` and NOT the app's own
    /// `tcrbar://`.
    public static let scheme = "tcr"
    /// `tcr://peer/moved`, which `URL` parses as the host `peer` and the path
    /// `/moved`.
    public static let host = "peer"
    public static let path = "/moved"

    /// Which of the two `tcr://peer/…` links a URL is.
    ///
    /// Decided in one function rather than at the two handlers, because the
    /// failure worth preventing is a URL that matches neither test at one call
    /// site and both at another. `neither` still reaches the join handler,
    /// which already has the sentence for a `tcr://` link this build does not
    /// know.
    public enum Route: Equatable, Sendable {
        case moved
        case join
        case neither
    }

    /// Why a URL was not a moved link. Named cases rather than `nil`, because
    /// the handler LOGS the refusal, and a link that quietly did nothing is
    /// indistinguishable from an app that is broken.
    public enum Refusal: Error, Equatable, Sendable {
        case notOurScheme(String?)
        case notTheMovedPath(host: String?, path: String)
        /// No `r` value. A link carrying nothing sealed has nothing for `tcr`
        /// to open, so running it would be a press that cannot work.
        case carriesNoRecord
    }

    /// The route a URL takes, and the only routing decision in this app.
    public static func route(_ url: URL) -> Route {
        guard url.scheme?.lowercased() == scheme, url.host?.lowercased() == host else {
            return .neither
        }
        switch trimmedPath(url) {
        case path: return .moved
        case PeerJoinLink.path: return .join
        default: return .neither
        }
    }

    /// Turn a URL into the one invocation that may run it.
    ///
    /// `apply` is the operator's own answer and the whole of the two-run flow:
    /// without it `tcr peer moved open` reads the link, says what it would
    /// add, and writes nothing. The preview run therefore carries no `--yes`,
    /// which is what makes the alert a decision rather than a notice about
    /// something that already happened.
    ///
    /// Checks the shape and NOTHING about the record's contents, for the
    /// reason in this type's own header: one decider, and it is `tcr`.
    public static func invocation(
        for url: URL, apply: Bool = false
    ) -> Result<PeerSecretInvocation, Refusal> {
        guard url.scheme?.lowercased() == scheme else {
            return .failure(.notOurScheme(url.scheme))
        }
        guard url.host?.lowercased() == host, trimmedPath(url) == path else {
            return .failure(.notTheMovedPath(host: url.host, path: url.path))
        }
        let items = URLComponents(url: url, resolvingAgainstBaseURL: false)?.queryItems ?? []
        let sealed = items.filter { $0.name == "r" && !($0.value ?? "").isEmpty }
        guard !sealed.isEmpty else { return .failure(.carriesNoRecord) }
        var arguments = ["peer", "moved", "open", "--stdin"]
        if apply {
            arguments.append("--yes")
        }
        return .success(
            PeerSecretInvocation(arguments: arguments, stdin: url.absoluteString))
    }

    /// What may be written to a log about a link: its shape, and whether it
    /// carried anything sealed. Never the record, never the URL.
    ///
    /// The handler has to say something, and the system log is readable by
    /// other processes on this Mac, so this is the only sentence it is given.
    public static func redacted(_ url: URL) -> String {
        let items = URLComponents(url: url, resolvingAgainstBaseURL: false)?.queryItems ?? []
        let carried = items.contains { $0.name == "r" && !($0.value ?? "").isEmpty }
        return "tcr://\(url.host ?? "?")\(url.path) with \(carried ? "a sealed record" : "nothing sealed")"
    }

    /// What `tcr peer moved open` answered on the run that writes nothing.
    ///
    /// Two outcomes and one input, the EXIT CODE. The words are carried
    /// through untouched either way: `tcr` decides whether a link is good, for
    /// whom, and how old is too old, and an app that re-read its sentences to
    /// classify them would be making that call a second time and disagreeing
    /// the first time the CLI gained a case.
    public enum Preview: Equatable, Sendable {
        /// The link was read. The string is what `tcr` printed, and it is what
        /// the person is shown before anything is kept.
        case wouldAdd(String)
        /// Nothing was kept and nothing will be. The string is what `tcr`
        /// said, or this app's own words when it said nothing at all.
        case refused(String)

        /// Whether the operator gets a button that keeps anything.
        ///
        /// A refused link is a sentence and a stop. It never turns into an
        /// offer to pair, to trust, or to add a Mac: a link forwarded into the
        /// wrong chat would then be a trust prompt in front of someone the
        /// sender never meant to ask.
        public var offersApply: Bool {
            if case .wouldAdd = self { return true }
            return false
        }

        /// The text to put on screen, whichever outcome this is.
        public var lines: String {
            switch self {
            case .wouldAdd(let text): return text
            case .refused(let text): return text
            }
        }
    }

    /// Classify the preview run.
    ///
    /// An exit of 0 with nothing printed is treated as a refusal in this app's
    /// own words rather than an empty alert, which would ask a person to agree
    /// to a blank page. That is the same answer ``PeerCommand`` callers already
    /// get from a capture that exits clean and prints nothing.
    public static func preview(exitCode: Int32, stdout: String, stderr: String) -> Preview {
        let out = stdout.trimmingCharacters(in: .whitespacesAndNewlines)
        let err = stderr.trimmingCharacters(in: .whitespacesAndNewlines)
        guard exitCode == 0 else {
            if !err.isEmpty { return .refused(err) }
            if !out.isEmpty { return .refused(out) }
            return .refused(
                "tcr peer moved open failed (exit \(exitCode)) and printed nothing, so nothing "
                    + "was kept.")
        }
        guard !out.isEmpty else {
            return .refused(
                "tcr peer moved open exited 0 and printed nothing, so there is nothing to show "
                    + "and nothing to keep.")
        }
        return .wouldAdd(out)
    }

    /// The sentence a refusal puts on screen or in the log.
    ///
    /// None of these name a Mac, an address or anything else off this Mac's
    /// peers file, and none of them echo the string that arrived. Two reasons.
    /// A link that opens against nothing here must teach whoever pasted it
    /// nothing about who this Mac knows, or a link forwarded into the wrong
    /// group chat becomes a way to ask that question. And the shape that DID
    /// arrive is already in the log through ``redacted(_:)``, where it belongs,
    /// rather than quoted back into a person's face as if it were the problem.
    public static func sentence(for refusal: Refusal) -> String {
        switch refusal {
        case .notOurScheme:
            return "not a tcr:// link, so nothing was read"
        case .notTheMovedPath:
            return "a tcr:// link this build does not read as a moved link; the moved one is "
                + "tcr://peer/moved"
        case .carriesNoRecord:
            return "that link carries nothing sealed, so there is nothing to open"
        }
    }

    /// The path with one trailing slash taken off.
    ///
    /// `tcr://peer/moved/` and `tcr://peer/moved` are the same link to anyone
    /// pasting one, and a chat client that adds the slash is not a refusal a
    /// person could see the reason for.
    private static func trimmedPath(_ url: URL) -> String {
        url.path.hasSuffix("/") ? String(url.path.dropLast()) : url.path
    }
}
