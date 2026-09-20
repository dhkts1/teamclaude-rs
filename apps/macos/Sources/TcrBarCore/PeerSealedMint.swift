import Foundation

/// What one run of `tcr peer invite --sealed`, `tcr peer join --stdin` fed an
/// ask, or `tcr peer invite --reply --stdin` answered.
///
/// A sibling of ``PeerInviteMint``, not a case added to it, for the reason
/// that type's own doc gives about ``PeerMovedMint``: the three runs print
/// different things and a shared case would have to know which verb ran. All
/// three still share one classifier because they share one sheet: whichever
/// blob is on stdout, or none, is the only thing that tells the three apart.
///
/// # Three blobs, or none
///
/// `tcr peer invite --sealed` prints an ask (`tcr-invite:v1:…`). `tcr peer
/// join --stdin`, fed an ask, prints a reply (`tcr-reply:v1:…`). `tcr peer
/// invite --reply --stdin`, opening that reply, prints no blob at all: it
/// joins on the spot and says so in a sentence, `peer invite: ok addr=… `.
/// So the classifier tries the ask prefix, then the reply prefix, and only
/// then falls back to "whatever this run printed is the joined sentences",
/// the same order a person reading the three verbs would try them in.
///
/// Nothing here reads a byte of what is sealed inside either blob, the same
/// rule ``PeerInviteMint`` states about the key it picks out.
public enum PeerSealedMint {
    /// One run, as the thing it was.
    public enum Outcome: Equatable, Sendable {
        /// An ask was minted: the blob, and whatever `tcr` said underneath
        /// it, in its own words.
        case asked(ask: String, sentences: String)
        /// A reply was sealed to an ask this Mac was handed: the blob, and
        /// `tcr`'s own sentences under it.
        case answered(reply: String, sentences: String)
        /// A reply was opened and the join it carried ran immediately: no
        /// blob, just `tcr`'s own sentences, `peer invite: ok addr=… `
        /// among them.
        case joined(sentences: String)
        /// `tcr` refused, and said why. Its sentence, unedited.
        case refused(String)
        /// The run itself did not happen or did not answer.
        case couldNotRun(String)
    }

    /// The start of the one printed line that is an ask: `ASK_PREFIX`,
    /// `src/peer/ask.rs`. Written out rather than read from Rust, the same as
    /// every other cross-language constant here;
    /// `PeersPanelWiringTests.testTheSealedSheetQuotesTheCliRatherThanRespellingIt`
    /// is what pins it against drift.
    public static let askPrefix = "tcr-invite:v1:"

    /// The start of the one printed line that is a reply: `REPLY_PREFIX`,
    /// `src/peer/ask.rs`. Pinned by the same test.
    public static let replyPrefix = "tcr-reply:v1:"

    /// True for the sentence `connect_in_key_order` bails with when a dial
    /// found nobody home (`src/peer/pair.rs:1019-1024`), false for every
    /// `ask::ReplyRefusal` sentence (`src/peer/ask.rs:258-278`), those being
    /// the reply that never opened at all.
    ///
    /// A spent dial has a remedy the sheet can offer, the invite works in
    /// the other direction too, and a reply that never opened does not: it
    /// is a stale, spent or misaddressed ask, and pressing anything again
    /// asks for a fresh one. Both reach the same enum case, so a view that
    /// drew the two apart itself would be the only reader of a fact `tcr`
    /// already prints; `theyDidNotAnswer` is that one reading, tested
    /// against the Rust sentence it keys on rather than a copy of it.
    public static func theyDidNotAnswer(_ said: String) -> Bool {
        said.contains("nothing answered at any address")
    }

    /// The reply screen's headline, said once here rather than by each of
    /// its two readers: `PeersSettingsView.pasteKeySheet`, which draws it
    /// today, and `PeerInviteSheet.sealedModeContent`, whose `answered` arm
    /// starts drawing it once the swap ships.
    public static let sendThisBackTitle = "Send this back"

    /// The reply screen's sentence, the same two readers as
    /// ``sendThisBackTitle``.
    public static let sendThisBackBody =
        "This carries your address, sealed so only the Mac that invited you can open it. "
        + "Nobody else who reads the message learns anything from it."

    /// Classify one run from its exit code and its two streams.
    ///
    /// The exit code leads, the same way ``PeerInviteMint/outcome(exitCode:stdout:stderr:)``
    /// does: a refusal is a refusal because the process said so, not because
    /// a sentence looked like one.
    public static func outcome(exitCode: Int32, stdout: String, stderr: String) -> Outcome {
        guard exitCode == 0 else {
            let said = PeerMovedLink.refusalWords(stdout: stdout, stderr: stderr)
            guard !said.isEmpty else {
                return .couldNotRun(
                    "that exited \(exitCode) and printed nothing, so there is no reason for "
                        + "the refusal.")
            }
            return .refused(said)
        }
        let lines = stdout.split(separator: "\n", omittingEmptySubsequences: false).map {
            $0.trimmingCharacters(in: .whitespaces)
        }
        if let ask = lines.first(where: { $0.hasPrefix(askPrefix) }) {
            return .asked(ask: ask, sentences: sentencesExcluding(ask, in: lines))
        }
        if let reply = lines.first(where: { $0.hasPrefix(replyPrefix) }) {
            return .answered(reply: reply, sentences: sentencesExcluding(reply, in: lines))
        }
        let sentences = lines.filter { !$0.isEmpty }.joined(separator: "\n")
        guard !sentences.isEmpty else {
            return .couldNotRun("that exited 0 without printing a blob or a sentence.")
        }
        return .joined(sentences: sentences)
    }

    /// Every non-empty line but the one already picked out, in order:
    /// ``PeerInviteMint/outcome(exitCode:stdout:stderr:)``'s own move.
    private static func sentencesExcluding(_ picked: String, in lines: [String]) -> String {
        lines.filter { $0 != picked && !$0.isEmpty }.joined(separator: "\n")
    }
}
