import Foundation

/// What one run of `tcr peer moved mint <peer>` answered.
///
/// The minting half of ``PeerMovedLink``, and the same division of labour:
/// `tcr` decides whether a link can be made, for whom, and what it is worth
/// saying about it, and this carries its words through untouched. Nothing here
/// writes a sentence about a link, an address, a Mac or how long a link stays
/// good. The one thing it decides is which of three things a run was, and it
/// decides that from the exit code and from whether a link was printed, never
/// by reading what a sentence says.
///
/// # Three outcomes, because a person needs three different screens
///
/// A run that produced a link, a run `tcr` refused and explained, and a run
/// this app could not make at all. The middle one is the case this feature has
/// a hard limit in: a pair that has not completed a session since the shared
/// secret existed has no key only those two Macs hold, and the CLI refuses
/// with the remedy in the sentence. That sentence is shown as it arrived. A
/// second spelling of it here would be a second sentence, free to drift from
/// the one the same person reads in a terminal.
///
/// # Why the printed link is picked out of stdout
///
/// The verb prints the link on its own line and then says two things about it,
/// and it may print a line about a router mapping it could not read before
/// either. What a person does with this screen is copy the link into a chat
/// window, so the box they copy from holds the link and nothing else. Picking
/// the line out is not parsing the record: nothing here reads a single byte of
/// what is sealed, which is ``PeerMovedLink``'s own rule.
public enum PeerMovedMint {
    /// One run, as the thing it was.
    public enum Outcome: Equatable, Sendable {
        /// A link, and whatever `tcr` said about it underneath, in its words.
        case minted(link: String, sentences: String)
        /// `tcr` would not make one, and said why. Its sentence, unedited.
        case refused(String)
        /// The run itself did not happen or did not answer: `tcr` was not
        /// found, the process failed, or it exited in a way that left nothing
        /// to show. This app's own words, because none of that is something
        /// `tcr` reported.
        case couldNotRun(String)
    }

    /// The start of the one printed line that is the link.
    ///
    /// Built from ``PeerMovedLink``'s own three parts rather than written out
    /// again, so the panel that RECOGNISES a link and the panel that OPENS one
    /// cannot end up disagreeing about what one looks like.
    public static var linkPrefix: String {
        "\(PeerMovedLink.scheme)://\(PeerMovedLink.host)\(PeerMovedLink.path)"
    }

    /// Classify one run from its exit code and its two streams.
    ///
    /// The exit code leads, the same way ``PeerMovedLink/answer(exitCode:stdout:stderr:)``
    /// does: a refusal is a refusal because the process said so, not because a
    /// sentence looked like one.
    public static func outcome(exitCode: Int32, stdout: String, stderr: String) -> Outcome {
        guard exitCode == 0 else {
            let said = PeerMovedLink.refusalWords(stdout: stdout, stderr: stderr)
            guard said.isEmpty else { return .refused(said) }
            return .couldNotRun(
                "tcr peer moved mint failed (exit \(exitCode)) and printed nothing, so there "
                    + "is no link and no reason for there not being one.")
        }
        let lines = stdout.split(separator: "\n", omittingEmptySubsequences: false).map {
            $0.trimmingCharacters(in: .whitespaces)
        }
        guard let link = lines.first(where: { $0.hasPrefix(linkPrefix) }) else {
            return .couldNotRun(
                "tcr peer moved mint exited 0 without printing a link, so there is nothing to "
                    + "send.")
        }
        let sentences = lines.filter { $0 != link && !$0.isEmpty }.joined(separator: "\n")
        return .minted(link: link, sentences: sentences)
    }
}
