import Foundation

/// The last thing `tcr` refused to do, held until the operator has read it.
///
/// # What it replaces
///
/// A failed verb used to be written into the SNAPSHOT, and the tab drew that
/// snapshot's failure INSTEAD of itself: the Find switch, the mesh, every row
/// and the Share switch disappeared, replaced by one card. Then the three
/// second poll's next read overwrote the snapshot and the card went too. So a
/// refusal destroyed every control on the tab to report itself, and then hid
/// the report about three seconds later, which is the worst possible pairing
/// of those two behaviours. The long refusal a mismatched pairing produces
/// landed here.
///
/// A refusal is therefore NOT part of the read. It is what this panel asked
/// for and was told no about, it lives beside the snapshot rather than in it,
/// and a poll cannot touch it.
///
/// # When it goes
///
/// Two ways, and no third: the operator dismisses it, or the next verb
/// succeeds. It does not expire, because there is no length of time after
/// which an operator who was looking away deserves to be told nothing.
public struct PeerRefusal: Equatable, Sendable {
    /// `tcr`'s own words, unparaphrased. `nil` when there is nothing to show.
    ///
    /// Kept whole and shown behind `Details`: it carries the command line and
    /// the exit code, which is exactly what a bug report needs and what a
    /// person reading a panel does not.
    public private(set) var message: String?

    /// The argv that was refused, when the caller knew it. `nil` from a
    /// failure this panel produced outside a verb.
    public private(set) var verb: [String]?

    public init(message: String? = nil, verb: [String]? = nil) {
        self.message = message
        self.verb = verb
    }

    /// A verb was refused. The newest refusal wins: it is the one the
    /// operator just caused.
    public mutating func refused(_ message: String, verb: [String]? = nil) {
        self.message = message
        self.verb = verb
    }

    /// A verb did what it was asked. Whatever was on screen is answered.
    public mutating func succeeded() {
        message = nil
        verb = nil
    }

    /// The operator read it.
    public mutating func dismissed() {
        message = nil
        verb = nil
    }

    public var isShowing: Bool { message != nil }

    /// What the banner LEADS with: `Sharing was refused`.
    ///
    /// The act, named, so a banner cannot say "that" about a press three
    /// seconds old on a tab with a dozen controls. A verb this build has no
    /// word for keeps the old sentence rather than inventing a noun from
    /// argv: `Vacuuming was refused` would be this panel guessing at a
    /// subcommand it does not know.
    public var headline: String {
        guard let noun = Act(arguments: verb)?.noun else { return "That was refused" }
        return "\(noun) was refused"
    }

    /// The half a person can act on, in the CLI's own words.
    ///
    /// The raw line is `tcr peer share on failed (exit 1): peer share:
    /// refused, no Mac is trusted yet, so there is nobody to share with`: a
    /// command line, an exit code and the verb again, and then, eighty
    /// characters in, the sentence. This is that sentence, sentence-cased and
    /// closed with a full stop, and NOTHING is added to it: a next step
    /// invented per refusal would be this panel advising on a failure it did
    /// not diagnose.
    ///
    /// A message in another shape is kept WHOLE. Trimming at a pattern a
    /// message does not have leaves a person with the wrong half.
    public var body: String? {
        guard let message, !message.isEmpty else { return nil }
        var text = message
        var trimmed = false
        // Everything up to and including the exit code is the machine's half.
        if let colon = text.range(of: "): ", options: .backwards) {
            text = String(text[colon.upperBound...])
            trimmed = true
        }
        // `peer share: refused, …`, the verb restated and the word the exit
        // code already carried.
        if let refused = text.range(of: "refused, ") {
            text = String(text[refused.upperBound...])
            trimmed = true
        }
        // Nothing was recognised, so nothing is re-cased or punctuated: a
        // message in another shape is somebody else's sentence, and
        // `tcr not found` sentence-cased reads `Tcr not found`, which is not
        // the name of the tool.
        guard trimmed else { return message }
        text = text.trimmingCharacters(in: .whitespacesAndNewlines)
        guard let first = text.first else { return message }
        let sentence = first.uppercased() + text.dropFirst()
        return sentence.hasSuffix(".") ? sentence : sentence + "."
    }

    /// The verbs this panel runs, as the nouns a person reads.
    ///
    /// A typed value over `argv[1]` rather than string comparisons at the
    /// call site, so a subcommand this build does not know is one `nil` and
    /// not an arm that falls through to another verb's noun.
    private enum Act: String {
        case share, find, block, unblock, accept, ignore, lend, join, invite, forget

        init?(arguments: [String]?) {
            guard let arguments, arguments.count > 1, let act = Act(rawValue: arguments[1])
            else { return nil }
            self = act
        }

        var noun: String {
            switch self {
            case .share: return "Sharing"
            case .find: return "Finding"
            case .block: return "Blocking"
            case .unblock: return "Unblocking"
            case .accept: return "Accepting"
            case .ignore: return "Ignoring"
            case .lend: return "Lending"
            case .join: return "Joining"
            case .invite: return "The invite"
            case .forget: return "Forgetting"
            }
        }
    }
}
