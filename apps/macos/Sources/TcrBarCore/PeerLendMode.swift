import Foundation

// "How <Mac> sends", the grant's own mode, and the third state a two-option
// control still needs.
//
// Per LEASE, not per Mac, on the lead's ruling ("row 15 puts `mode` on the
// grant … a Mac with two grants can have one of each"), so the control lives
// on the lease being edited and every sentence here takes a grant.

/// How one grant's requests leave: over the owner's Mac, or on the borrower's
/// own with a key the owner keeps renewing.
///
/// The wire spelling is `src/peer/config.rs`'s `LendMode`, camelCase, and it
/// is SKIPPED when it is `serve`, which is what every grant written before
/// modes existed already meant. So absent decodes to ``serve`` here too, and
/// a grant that never asked for a handed key draws the default rather than an
/// unknown.
public enum LendMode: String, Codable, Equatable, Hashable, CaseIterable, Sendable {
    case serve
    case hand

    /// The label on the segmented control. Never the wire word: `serve` and
    /// `hand` are the file's vocabulary, and what changes for the operator is
    /// who does the sending.
    public var label: String {
        switch self {
        case .serve: return "Over this Mac"
        case .hand: return "With a handed key"
        }
    }

    /// The argv value, which IS the wire word.
    public var argument: String { rawValue }

    /// A mode this build does not know decodes as ``serve``, the shipped
    /// default, rather than throwing away the grant: the mode is one field of
    /// a lease whose other numbers the sheet still has to draw.
    public init(token: String?) {
        self = LendMode(rawValue: token ?? "serve") ?? .serve
    }
}

extension PeerLease {
    /// The sentence under the control, which changes ENTIRELY between the two
    /// modes rather than swapping one word: the fact that changes is who reads
    /// the request, and that is not a cosmetic difference.
    ///
    /// It opens with the FACT, never with its own segment's label. Each arm
    /// began `Over this Mac (now).` and `With a handed key (now).`, directly
    /// under a segmented control already showing the selected one of those two
    /// labels: the first words of the explaining sentence said only what the
    /// control above it had said, and the parenthetical said nothing at all.
    public static func modeSentence(_ mode: LendMode, peer: String) -> String {
        switch mode {
        case .serve:
            return "\(peer)'s requests travel through you and leave on your own address; you "
                + "see each one before it is sent. Turning this off stops it immediately, "
                + "mid request if one is running."
        case .hand:
            return "\(peer) holds a short lived key and sends its own requests, on its own "
                + "address; you never see what it asks. You keep renewing the key behind the "
                + "scenes, and to stop it, you just stop renewing."
        }
    }

    /// The status line under that sentence: whether the key is actually being
    /// renewed right now, or whether an old one is still winding down.
    ///
    /// # Why it is a THIRD state on a two-option control
    ///
    /// Decision row 15: "to revoke, the owner just stops renewing." So
    /// switching back to serve is not instant the way a switch usually is, the
    /// borrower's key stays good until it expires on its own. Without this
    /// line an operator who switched away because they no longer trust that
    /// Mac would believe access ended the instant they clicked. It has not.
    ///
    /// `nil` when there is nothing to say, and that includes the case where
    /// the producer sent no handed-key expiry at all: an absent field is an
    /// absence, never a guessed countdown.
    public static func handedKeyLine(_ grant: PeerLendGrant, peer: String, now: Date)
        -> HandedKeyLine?
    {
        handedKeyLine(
            mode: grant.mode, handedKeyUntil: grant.handedKeyUntil, peer: peer, now: now)
    }

    /// The same line from the two fields it is made of, so the sheet can draw
    /// it for a lease being EDITED (a draft holds a mode, not a grant) without
    /// a second spelling of the rule.
    public static func handedKeyLine(
        mode: LendMode, handedKeyUntil: Int64?, peer: String, now: Date
    ) -> HandedKeyLine? {
        guard let until = handedKeyUntil else {
            // A hand-mode grant whose producer says nothing about the key it
            // handed: the mode is still worth reporting, the renewal is not
            // something this build measured.
            return mode == .hand
                ? HandedKeyLine(text: "Renewing normally", winding: false) : nil
        }
        let seconds = Double(until) - now.timeIntervalSince1970
        switch mode {
        case .hand:
            guard seconds > 0 else {
                return HandedKeyLine(
                    text: "The key it holds has expired; the next renewal replaces it",
                    winding: true)
            }
            return HandedKeyLine(
                text: "Renewing normally, this key lasts another \(PeerFormat.span(seconds))",
                winding: false)
        case .serve:
            // Switched back, and the old key outlives the press. THE state the
            // mockup's scene 2c exists for.
            guard seconds > 0 else { return nil }
            return HandedKeyLine(
                text: "\(peer)'s old key is not renewing and expires on its own in "
                    + "\(PeerFormat.span(seconds))",
                winding: true)
        }
    }

    /// One status line and whether it is the amber, winding-down one. A pair
    /// rather than two calls, because a caller that can ask for the text
    /// without the tint can draw a winding key in the healthy colour.
    public struct HandedKeyLine: Equatable, Sendable {
        public let text: String
        public let winding: Bool

        public init(text: String, winding: Bool) {
            self.text = text
            self.winding = winding
        }
    }
}
