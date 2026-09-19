import Foundation

// The machine half of `tcr peer pair`, as values the sheet can draw and a test
// can drive without a subprocess.
//
// # Why this exists at all
//
// The pairing is ONE live handshake held across both phases in ONE process:
// the far side's Accept opens a window for this instance id, the handshake
// that follows produces the six digits, and the pin is written from the same
// process because a half-finished handshake cannot be persisted and a second
// invocation has nothing to recompute the code from (`src/peer/pair.rs`). So
// this panel cannot run the pairing as two fire-and-forget verbs. It has to
// start the command, stay attached to it, read what it says, and answer on its
// stdin.
//
// What it reads is `tcr peer pair --json`: one JSON object per line, each
// naming its own `event`. Never the prose the same command prints for a
// person. A panel that scraped "peer pair: this Mac shows 418902" would break
// on the next wording change with every gate on both sides still green, which
// is the class of defect this file is the fix for.

/// One line of `tcr peer pair --json`.
///
/// `src/peer/pair.rs`'s `PairEvent`, and `tests/peer_pairing.rs` pins the exact
/// bytes of all four lines, because what breaks a decode is a renamed key
/// rather than a wrong value: `waitSeconds` becoming `wait_seconds` decodes to
/// nothing and the sheet simply never leaves its first state.
public enum PeerPairEvent: Equatable, Sendable {
    /// The knock is away. Nothing happens until somebody at the other Mac
    /// presses Accept, which can be ten minutes from now.
    case asking(addr: String, instance: String, waitSeconds: Int)
    /// The handshake produced six digits. `code` is what THIS Mac shows; the
    /// operator reads the other screen and types those.
    case comparing(code: String)
    /// The compared digits matched and the key is pinned.
    case trusted(peer: String)
    /// The pairing ended with no pin, in the CLI's own words.
    case refused(message: String)

    /// Decode one line, or `nil` when the line is not one of these.
    ///
    /// `nil` rather than a throw, and the caller keeps reading: a stray line on
    /// stdout (a warning from a library, a newer `tcr`'s fifth event) must not
    /// end a pairing a person is standing in front of. It is not a silent
    /// fallback either, because nothing is invented from it: the state machine
    /// moves on events it understood and on nothing else, and a run that ends
    /// having understood nothing reports exactly that
    /// (``PeerPairState/refused(_:)`` with the process's own words).
    public static func decode(line: String) -> PeerPairEvent? {
        let trimmed = line.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty, let data = trimmed.data(using: .utf8) else { return nil }
        return try? JSONDecoder().decode(PeerPairEvent.self, from: data)
    }
}

extension PeerPairEvent: Decodable {
    private enum Keys: String, CodingKey {
        case event, addr, instance, waitSeconds, code, peer, message
    }

    /// The tag values `src/peer/pair.rs` writes. A typed enum rather than four
    /// string comparisons, so a tag this build does not know is one refused
    /// decode and not an arm that falls through to another event's shape.
    private enum Tag: String, Decodable {
        case asking, comparing, trusted, refused
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: Keys.self)
        switch try c.decode(Tag.self, forKey: .event) {
        case .asking:
            self = .asking(
                addr: try c.decode(String.self, forKey: .addr),
                instance: try c.decode(String.self, forKey: .instance),
                waitSeconds: try c.decode(Int.self, forKey: .waitSeconds))
        case .comparing:
            self = .comparing(code: try c.decode(String.self, forKey: .code))
        case .trusted:
            self = .trusted(peer: try c.decode(String.self, forKey: .peer))
        case .refused:
            self = .refused(message: try c.decode(String.self, forKey: .message))
        }
    }
}

/// What the Trust sheet is drawing, at any instant of one pairing.
///
/// Five states and no sixth, and the sheet switches on this rather than on a
/// `code: String?`. That optional IS the blocker this type replaces: the sheet
/// was built with a literal `nil` at its only call site and its Trust button
/// read `enabled: code != nil`, so the control could never be pressed and the
/// pairing could not be finished from the panel at all.
public enum PeerPairState: Equatable, Sendable {
    /// The knock is away and nobody at the other Mac has answered yet.
    /// Carries what that Mac's operator has to do, by name, because "waiting"
    /// with no instruction is a dead end for the person standing at the other
    /// screen.
    case asking(instance: String)
    /// Six digits on this screen, and the operator is reading the other one.
    case comparing(code: String)
    /// Pinned.
    case done(peer: String)
    /// No pin, with the reason as `tcr` gave it. Never paraphrased.
    case refused(String)
    /// The operator closed the sheet and the child was terminated.
    case cancelled

    /// Whether this state can still change by itself. A finished run's sheet
    /// stops claiming anything is in flight.
    public var isLive: Bool {
        switch self {
        case .asking, .comparing: return true
        case .done, .refused, .cancelled: return false
        }
    }

    /// The instruction for the OTHER Mac, while this one waits. `nil` once
    /// there is nothing over there left to do, and `nil` before the command
    /// has named the instance: a sentence with a blank where an argument
    /// belongs is worse than no sentence.
    ///
    /// The instance id and not the name: `tcr peer accept` takes the id, and
    /// two Macs can propose one name.
    public var farSideInstruction: String? {
        guard case .asking(let instance) = self, !instance.isEmpty else { return nil }
        return "On that Mac, `tcr peer pending` lists this request and "
            + "`tcr peer accept \(instance)` approves it. Nothing has been disclosed to it yet."
    }

    /// The sheet's headline for this state. The words live here rather than in
    /// the view for the reason ``PeerAdmission`` gives for the knock's: a
    /// sentence a test can read is a sentence that cannot quietly start
    /// claiming something the pairing has not done.
    public func title(peerName: String) -> String {
        switch self {
        case .asking: return PeerAdmission.waitingTitle(name: peerName)
        case .comparing: return "Compare the digits with \(peerName)"
        case .done: return "\(peerName) is trusted"
        case .refused: return "\(peerName) was not trusted"
        case .cancelled: return "Stopped pairing with \(peerName)"
        }
    }

    /// What the sheet says under the headline.
    ///
    /// The comparing sentence is the security property of this whole ritual
    /// written down: the operator READS the other screen. It never says the
    /// two numbers match, because only the person looking at both can know
    /// that, and a sheet that asserted it would be teaching them to press past
    /// the one check the six digits exist for.
    public func sentence(peerName: String) -> String {
        switch self {
        case .asking:
            return PeerAdmission.waitingSentence
        case .comparing:
            return "\(peerName) is showing six digits of its own. Read them off that screen and "
                + "type them here: if one digit differs, stop, because something is answering "
                + "in its place. A match pins its key and it can carry your traffic without "
                + "reading it."
        case .done(let peer):
            return "Its key is pinned here as \(peer). It can carry your encrypted bytes and "
                + "open none of them; nothing is shared until you turn sharing on. Run the same "
                + "pairing on that Mac, pointed back here, so both sides hold a pin."
        case .refused(let message):
            return message
        case .cancelled:
            return "Nothing was pinned and \(peerName) stays untrusted. The request may still "
                + "be standing on that Mac until somebody answers it or it expires."
        }
    }

    /// Fold one event into the state.
    ///
    /// Pure, and separate from the process that produces the events, so the
    /// whole state machine is driven by a test with no subprocess anywhere:
    /// the run below is then only pipes and a deadline.
    ///
    /// A finished run ignores everything after it. `trusted` and `refused` are
    /// terminal on the CLI's side too, and a late line moving a sheet off
    /// "pinned" would be this panel inventing a state the pairing does not
    /// have.
    public func applying(_ event: PeerPairEvent) -> PeerPairState {
        guard isLive else { return self }
        switch event {
        case .asking(_, let instance, _): return .asking(instance: instance)
        case .comparing(let code): return .comparing(code: code)
        case .trusted(let peer): return .done(peer: peer)
        case .refused(let message): return .refused(message)
        }
    }

    /// How long this panel may wait on the command, from the command's own
    /// first line. `nil` from any other event, so a deadline is never invented
    /// here.
    ///
    /// Taken off the wire rather than written down: `PAIR_WAIT` is ten minutes
    /// today and a second copy of that number in Swift is one the next edit
    /// can move without moving this one, leaving the panel either cancelling a
    /// live handshake or sitting on a process that has already gone.
    public static func wait(from event: PeerPairEvent) -> TimeInterval? {
        guard case .asking(_, _, let seconds) = event else { return nil }
        return TimeInterval(seconds)
    }
}

/// The six digits the operator reads off the OTHER Mac and types here.
///
/// A typed value rather than a bare `String` on the view, because the security
/// property of this whole ritual is that a PERSON compares two numbers. A
/// "they match" button would let an operator confirm without ever looking at
/// the other screen, so the panel asks for the digits themselves and the CLI
/// does the compare against the handshake it is still holding.
public struct PeerPairCompare: Equatable, Sendable {
    /// Exactly what is in the field: digits only, at most six.
    public private(set) var typed: String

    public init(typed: String = "") {
        self.typed = Self.digits(typed)
    }

    /// Take what was typed and keep only the part that can be a code: digits,
    /// six of them. A field that accepted `418-902` and sent it would be
    /// refused by the CLI as "not six digits", which reads as a mismatch and
    /// is not one.
    public mutating func set(_ raw: String) {
        typed = Self.digits(raw)
    }

    private static func digits(_ raw: String) -> String {
        String(raw.filter(\.isNumber).prefix(6))
    }

    /// Whether there is something to send. Six digits, which is the only
    /// length the CLI accepts.
    public var isComplete: Bool { typed.count == 6 }

    /// What goes down the child's stdin: the digits and the newline
    /// `read_line` is waiting for. Empty until the field is complete, so a
    /// half-typed code cannot be submitted by any path.
    public var submission: String? {
        guard isComplete else { return nil }
        return typed + "\n"
    }
}
