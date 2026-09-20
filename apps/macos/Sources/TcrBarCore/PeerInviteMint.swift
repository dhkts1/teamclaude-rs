import Foundation

/// What one run of `tcr peer invite` answered.
///
/// The minting half of a join key, and the same division of labour
/// ``PeerMovedMint`` already uses: `tcr` decides whether a key can be made,
/// what paths it carries, and what is worth saying about it, and this carries
/// its words through untouched. Nothing here writes a sentence about a path,
/// a Mac or how long a key stays good. The one thing it decides is which of
/// three things a run was, and it decides that from the exit code and from
/// whether a key was printed, never by reading what a sentence says.
///
/// # Three outcomes, because a person needs three different screens
///
/// A run that produced a key, a run `tcr` refused and explained (no listener,
/// no address to reach, the eight-outstanding cap), and a run this app could
/// not make at all. The first two both have a remedy inside the sentence; the
/// third is about this Mac's own installation and is not something `tcr`
/// reported.
///
/// # Why the printed key is picked out of stdout
///
/// The verb prints the key on its own line and then says several things about
/// it: one line per address it carries, in dial order, and how long it lasts.
/// What a person does with this screen is copy the key into a chat window, so
/// the box they copy from holds the key and nothing else. Picking the line
/// out is not parsing the token: nothing here reads a single byte of what is
/// sealed inside it.
public enum PeerInviteMint {
    /// One run, as the thing it was.
    public enum Outcome: Equatable, Sendable {
        /// A key, and whatever `tcr` said about it underneath, in its words.
        case minted(key: String, sentences: String)
        /// `tcr` would not make one, and said why. Its sentence, unedited.
        case refused(String)
        /// The run itself did not happen or did not answer: `tcr` was not
        /// found, the process failed, or it exited in a way that left nothing
        /// to show. This app's own words, because none of that is something
        /// `tcr` reported.
        case couldNotRun(String)
    }

    /// The start of the one printed line that is the key: `KEY_PREFIX`,
    /// `src/peer/pair.rs`. Written out rather than read from Rust, the same
    /// as every other cross-language constant here; `PeersPanelWiringTests`
    /// is what pins it against drift.
    public static let keyPrefix = "tcr-join:"

    /// Classify one run from its exit code and its two streams.
    ///
    /// The exit code leads, the same way ``PeerMovedMint/outcome(exitCode:stdout:stderr:)``
    /// does: a refusal is a refusal because the process said so, not because
    /// a sentence looked like one.
    public static func outcome(exitCode: Int32, stdout: String, stderr: String) -> Outcome {
        guard exitCode == 0 else {
            let said = PeerMovedLink.refusalWords(stdout: stdout, stderr: stderr)
            guard said.isEmpty else { return .refused(said) }
            return .couldNotRun(
                "tcr peer invite failed (exit \(exitCode)) and printed nothing, so there is no "
                    + "key and no reason for there not being one.")
        }
        let lines = stdout.split(separator: "\n", omittingEmptySubsequences: false).map {
            $0.trimmingCharacters(in: .whitespaces)
        }
        guard let key = lines.first(where: { $0.hasPrefix(keyPrefix) }) else {
            return .couldNotRun(
                "tcr peer invite exited 0 without printing a key, so there is nothing to send.")
        }
        let sentences = lines.filter { $0 != key && !$0.isEmpty }.joined(separator: "\n")
        return .minted(key: key, sentences: sentences)
    }
}
