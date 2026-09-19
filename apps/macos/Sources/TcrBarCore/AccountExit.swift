import Foundation

// "Exits from", on an Accounts-tab card.
//
// The on-screen name is the lead's ruling: the account's field is `egress` in
// `src/config.rs`, and that word never appears on this screen, because the
// same tab strip already uses it for a different control (routing this Mac's
// own outbound traffic through a peer). One word naming two switches on one
// surface is the collision the mockup's own rule S11 had to fix once already.

/// Where one account's requests leave from, and whether that is a promise or a
/// preference.
///
/// Parsed once, here, from the two keys `src/config.rs` already owns
/// (`egress: "local" | "via <peer id>"` and `egressStrict`), so every consumer
/// reads one typed value rather than re-matching the string. A route this
/// build cannot parse keeps its raw words rather than being called local: this
/// panel must never report that traffic leaves from this Mac when the file
/// says something else.
public struct AccountExit: Decodable, Equatable, Sendable {
    public enum Route: Equatable, Sendable {
        /// The default, and what an account with no `egress` key means.
        case local
        /// Out through a trusted Mac. **``Route(egress:)``, every route this
        /// type decodes off the wire, carries the WIRE id**, `config.rs`'s
        /// `via <peer id>`, never a name: parsing happens with no `peers`
        /// document in reach to resolve one against. ``label(peers:)`` is
        /// what turns a decoded route into the name the operator knows.
        /// A picker MAY still build `.via` locally with a name already in
        /// hand (choosing from a menu it populated itself) on its way to
        /// argv, since `--exits-from` accepts either: this case only
        /// promises what ``Route(egress:)`` puts in it.
        case via(String)
        /// A spelling this build does not know, carried verbatim.
        case unknown(String)

        /// `local`, `via <peer>`, as the config file spells it.
        public init(egress: String?) {
            guard let raw = egress?.trimmingCharacters(in: .whitespaces), !raw.isEmpty else {
                self = .local
                return
            }
            if raw == "local" {
                self = .local
            } else if raw.hasPrefix("via ") {
                let peer = String(raw.dropFirst(4)).trimmingCharacters(in: .whitespaces)
                self = peer.isEmpty ? .unknown(raw) : .via(peer)
            } else {
                self = .unknown(raw)
            }
        }

        /// The value this route carries: `"This Mac"`, the WIRE id `.via`
        /// holds, or `.unknown`'s raw spelling. **Not what the picker draws:**
        /// a `.via` id here is the 52-character wire form, never a name,
        /// because resolving one needs the `peers` document this type does
        /// not hold. ``label(peers:)`` is the picker's read.
        public var label: String {
            switch self {
            case .local: return "This Mac"
            case .via(let peer): return peer
            case .unknown(let raw): return raw
            }
        }

        /// What the picker reads: the peer's name, joined through `peers`,
        /// the `PeerListDocument` this route came from, by matching
        /// ``PeerListDocument/PeerEntry/id``. No row matches, or the matched
        /// row has no name yet, falls back to ``maskedPeerId(_:)`` and NEVER
        /// the raw id: this repo is public and every screenshot of this
        /// picker is too.
        public func label(peers: [PeerListDocument.PeerEntry]) -> String {
            switch self {
            case .local, .unknown: return label
            case .via(let id):
                if let name = peers.first(where: { $0.id == id })?.name {
                    return name
                }
                return Self.maskedPeerId(id)
            }
        }

        /// The value the argv carries, which IS the file's own spelling.
        public var argument: String {
            switch self {
            case .local: return "local"
            case .via(let peer): return peer
            case .unknown(let raw): return raw
            }
        }

        /// The Mac named, when one is. The WIRE id, same caveat as ``label``.
        public var peer: String? {
            if case .via(let peer) = self { return peer }
            return nil
        }

        /// A peer id, shortened for the screen. `"tcr-" + the first ten
        /// characters`, the same shape `PeerId::display()` gives it on the
        /// Rust side (`crates/tcr-peer-wire/src/lib.rs`), enough to read
        /// aloud, never enough to reconstruct the key, and this repo is
        /// public so the full 52-character wire form never belongs on
        /// screen.
        public static func maskedPeerId(_ id: String) -> String {
            "tcr-" + id.prefix(10)
        }
    }

    public var route: Route
    /// `egressStrict`: requests wait rather than leave from a second address.
    public var strict: Bool
    /// Whether the exit Mac is unreachable right now, as the PRODUCER reported
    /// it. Not derived from a last-seen age here: two clocks reading one fact
    /// is how a card and a row end up disagreeing.
    public var peerDown: Bool
    /// How long this account's requests have been waiting, seconds. `nil` when
    /// nothing reported a wait, which draws no figure rather than a zero.
    public var waitingSeconds: Double?

    public init(
        route: Route, strict: Bool = false, peerDown: Bool = false,
        waitingSeconds: Double? = nil
    ) {
        self.route = route
        self.strict = strict
        self.peerDown = peerDown
        self.waitingSeconds = waitingSeconds
    }

    /// The wire shape, which is the config file's own two keys plus the two
    /// live facts only a running proxy knows.
    ///
    /// Permissive like every other peer decode here: an absent `egress` is
    /// local, the default an account with no key already has, and an absent
    /// wait draws no figure.
    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: Keys.self)
        self.route = Route(egress: try c.decodeIfPresent(String.self, forKey: .egress))
        self.strict = try c.decodeIfPresent(Bool.self, forKey: .egressStrict) ?? false
        self.peerDown = try c.decodeIfPresent(Bool.self, forKey: .peerDown) ?? false
        self.waitingSeconds = try c.decodeIfPresent(Double.self, forKey: .waitingSeconds)
    }

    private enum Keys: String, CodingKey {
        case egress, egressStrict, peerDown, waitingSeconds
    }

    /// Whether the "must" switch is drawn at all.
    ///
    /// It appears WITH the picker naming a Mac and not before: strictness only
    /// means something once there is a Mac to be strict about, and a disabled
    /// switch on the default row is a control that answers a question nobody
    /// has asked yet.
    public var showsMust: Bool { route.peer != nil }

    /// THE state the lead ruled in: a must-locked account whose exit Mac is
    /// down. A pill that names the blocked Mac is the one detail that lets an
    /// operator go and fix the actual problem instead of guessing.
    ///
    /// **The WIRE id, not the name:** same caveat as ``AccountExit/Route/label``.
    /// ``waitingPill(peers:)`` is what the picker draws.
    public var waitingPill: String? {
        guard strict, peerDown, let peer = route.peer else { return nil }
        return "waiting for \(peer)"
    }

    /// What the picker draws: the same pill, with the peer's name in place of
    /// the wire id, resolved the way ``AccountExit/Route/label(peers:)`` is.
    public func waitingPill(peers: [PeerListDocument.PeerEntry]) -> String? {
        guard strict, peerDown, route.peer != nil else { return nil }
        return "waiting for \(route.label(peers: peers))"
    }

    /// The note under the row: what this state costs, in the operator's own
    /// terms. `nil` on the default, which needs no explanation: most accounts
    /// never need to look like they always come from one place.
    ///
    /// **The WIRE id, not the name:** same caveat as ``AccountExit/Route/label``.
    /// ``note(peers:)`` is what the picker draws.
    public var note: String? { note(name: route.peer) }

    /// What the picker draws: the same note, with the peer's name in place of
    /// the wire id, resolved the way ``AccountExit/Route/label(peers:)`` is.
    public func note(peers: [PeerListDocument.PeerEntry]) -> String? {
        guard route.peer != nil else { return nil }
        return note(name: route.label(peers: peers))
    }

    /// The note's own three sentences, parameterized on what to call the
    /// peer, the one thing ``note`` and ``note(peers:)`` disagree about.
    private func note(name peer: String?) -> String? {
        guard let peer else { return nil }
        guard strict else {
            return "If \(peer) is unreachable, these requests use this Mac instead, so the "
                + "address they leave from can change without warning."
        }
        guard peerDown else {
            return "If \(peer) is unreachable, these requests wait rather than leave from a "
                + "different address. They fail, not reroute, until \(peer) is back."
        }
        let waited = waitingSeconds.map { " Waiting \(PeerFormat.span($0))." } ?? ""
        return "\(peer) is down. These requests are refused, not sent from a different "
            + "address, until it is back.\(waited)"
    }

    /// Whether the note is the amber one: a promise being kept the hard way,
    /// or being tested right now.
    public var noteIsWarning: Bool { strict && route.peer != nil }
}

extension PeerCommand {
    /// `tcr peer account <label> --exits-from local|<peer> [--must|--no-must]`.
    ///
    /// The switch's argv is the state it MOVES to, like every other switch on
    /// these surfaces, so a must that is on writes `--no-must`.
    ///
    /// The verb exists (`run_peer_account`, `src/main.rs`; the two keys it
    /// writes, `egress` and `egressStrict`, are set through
    /// `cli::set_account_egress`). `--exits-from` takes the peer's NAME or
    /// its id: `run_peer_account` resolves a name against the pinned rows
    /// itself and refuses an ambiguous one, so ``AccountExit/Route/argument``
    /// carries whichever this route already holds, never a name this type
    /// invents.
    public static func accountExit(
        account: String, route: AccountExit.Route, must: Bool
    ) -> [String] {
        ["peer", "account", account, "--exits-from", route.argument]
            + [must ? "--must" : "--no-must"]
    }
}
