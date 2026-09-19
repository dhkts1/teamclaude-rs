import Foundation

// The "Reachable from the internet" switch on Settings > Peers > This Mac,
// and the one state line under it.
//
// Pure data, in `TcrBarCore` for the reason every other peer model here gives:
// the test target links this module alone, so a sentence an operator reads is
// a value a test can build rather than source text a grep has to match.

/// What `tcr peer reach --json` answered, as this app reads it.
///
/// `externalAddress`, `listenPort` and `mapping` are this call's own probe
/// outcome; `heldMapping` is a different thing entirely: the mapping a
/// SERVING process (the running proxy's keeper) currently holds, read by
/// `run_peer_reach` off its state file rather than asked of the router
/// again (see `src/main.rs`'s `run_peer_reach`). `heldMapping.expiresAtMs`
/// is an ABSOLUTE Unix-millisecond instant the keeper itself recorded, not
/// a lifetime this app has to add to its own clock, so it is what decides
/// ``PeerInternetReach``, not `mapping`'s parsed lifetime. `readAt` is kept
/// for the caller's own bookkeeping; it is no longer part of the expiry
/// computation.
public struct PeerReachReading: Equatable, Sendable {
    /// What the router says this Mac looks like from outside, or `nil` when
    /// the verb answered `unavailable: …`, which is an absence, never an
    /// address to print.
    public var externalAddress: String?
    /// The port the peer listener is bound to, or `nil` when it is off.
    public var listenPort: Int?
    public var mapping: Mapping
    /// The mapping a serving process currently holds, or `nil` when no
    /// serving process on this Mac holds one right now. See `MappingRecord`
    /// in `src/peer/state.rs` for the wire shape this decodes.
    public var heldMapping: HeldMapping?
    /// When this app received the answer. Its own clock; the payload also
    /// carries no instant for `mapping`'s lifetime, but `heldMapping` does
    /// not need one.
    public var readAt: Date

    /// The mapping a serving process holds, as `tcr` recorded it: see
    /// `MappingRecord` (`src/peer/state.rs`, `#[serde(rename_all =
    /// "camelCase")]`).
    public struct HeldMapping: Equatable, Sendable {
        /// Absent when the router mapped the port but would not name its own
        /// external address, the same "absent rather than a guess" rule
        /// `MappingRecord::external_address` states.
        public var externalAddress: String?
        public var externalPort: Int
        public var internalPort: Int
        /// Unix milliseconds. Absolute, not a duration: compare directly to
        /// `now`, never add it to `readAt`.
        public var expiresAtMs: Int

        public init(
            externalAddress: String?, externalPort: Int, internalPort: Int, expiresAtMs: Int
        ) {
            self.externalAddress = externalAddress
            self.externalPort = externalPort
            self.internalPort = internalPort
            self.expiresAtMs = expiresAtMs
        }

        /// `expiresAtMs` as a `Date`, the one place the millisecond-to-second
        /// conversion happens.
        public var expires: Date {
            Date(timeIntervalSince1970: Double(expiresAtMs) / 1000)
        }
    }

    /// The `mapping` string, parsed into the four outcomes the verb can
    /// print.
    ///
    /// Kept as cases rather than the raw string because the state line asks
    /// two different questions of it ("did the router agree" and "for how
    /// long"), and a caller re-matching the prose at each site is the drift
    /// this type exists to prevent. An unrecognised string is
    /// ``Mapping/refused(_:)`` with its own words, never silently a success:
    /// this app does not paraphrase `tcr`, and a sentence it cannot read is
    /// not evidence a port is open.
    public enum Mapping: Equatable, Sendable {
        /// `tcp 51413 -> 51413 for 120s`.
        case granted(externalPort: Int, lifetimeSeconds: Int)
        /// `refused: …`, or any answer this build cannot read.
        case refused(String)
        /// `not asked for (pass --map)`, the read-only call.
        case notAsked
        /// `unavailable: …`, or no listener to map.
        case unavailable(String)
    }

    public init(
        externalAddress: String?, listenPort: Int?, mapping: Mapping,
        heldMapping: HeldMapping? = nil, readAt: Date
    ) {
        self.externalAddress = externalAddress
        self.listenPort = listenPort
        self.mapping = mapping
        self.heldMapping = heldMapping
        self.readAt = readAt
    }

    /// Decode one `tcr peer reach --json` payload.
    ///
    /// Permissive in the same way ``PeerListDocument`` is: a missing key is an
    /// absence the line draws honestly, never a default that reads like a
    /// measurement. `externalAddress` in particular arrives as the STRING
    /// `unavailable: no gateway` on a Mac whose router did not answer at all,
    /// and that is `nil` here rather than an address beginning with the word
    /// "unavailable".
    public static func decode(_ data: Data, readAt: Date) throws -> PeerReachReading {
        let top = try JSONSerialization.jsonObject(with: data)
        guard let object = top as? [String: Any] else {
            throw DecodingError.dataCorrupted(
                .init(
                    codingPath: [],
                    debugDescription:
                        "tcr peer reach --json emits a JSON object; got \(type(of: top))"))
        }
        let raw = object["externalAddress"] as? String
        let address = raw.flatMap { text -> String? in
            let trimmed = text.trimmingCharacters(in: .whitespaces)
            guard !trimmed.isEmpty, !trimmed.hasPrefix("unavailable") else { return nil }
            return trimmed
        }
        return PeerReachReading(
            externalAddress: address,
            listenPort: object["listenPort"] as? Int,
            mapping: mapping(object["mapping"] as? String),
            heldMapping: heldMapping(object["heldMapping"]),
            readAt: readAt)
    }

    /// `heldMapping`, absent when the key is missing or `null` (no serving
    /// process holds one) or when a required field this build must have
    /// (`externalPort`, `internalPort`, `expiresAtMs`) is not the shape
    /// `MappingRecord` promises: a partial record is not a mapping this app
    /// can act on.
    static func heldMapping(_ value: Any?) -> HeldMapping? {
        guard let object = value as? [String: Any],
            let externalPort = object["externalPort"] as? Int,
            let internalPort = object["internalPort"] as? Int,
            let expiresAtMs = object["expiresAtMs"] as? Int
        else { return nil }
        return HeldMapping(
            externalAddress: object["externalAddress"] as? String,
            externalPort: externalPort, internalPort: internalPort, expiresAtMs: expiresAtMs)
    }

    /// `tcp 51413 -> 51413 for 120s` and the three refusals beside it.
    static func mapping(_ text: String?) -> Mapping {
        guard let text = text?.trimmingCharacters(in: .whitespaces), !text.isEmpty else {
            return .refused("tcr peer reach reported no mapping at all")
        }
        if text.hasPrefix("not asked for") { return .notAsked }
        if text.hasPrefix("unavailable") { return .unavailable(text) }
        if text.hasPrefix("no peer listener") { return .unavailable(text) }
        if text.hasPrefix("tcp ") {
            // `tcp <external> -> <internal> for <lifetime>s`, the one shape
            // `run_peer_reach` prints on a grant. Read by position rather than
            // by a regular expression so an added clause on the end cannot
            // turn a grant into a refusal.
            let words = text.split(separator: " ")
            if words.count >= 6, let external = Int(words[1]),
                let lifetime = Int(words[5].dropLast())
            {
                return .granted(externalPort: external, lifetimeSeconds: lifetime)
            }
        }
        return .refused(text)
    }
}

/// The state the switch's line is in, and the only thing that decides what it
/// says.
///
/// Four states, not two, and the extra pair is the whole point:
///
///  - ``off`` draws NO line. There is no port and no address to report, and a
///    blank line under an off switch reads as a fact rather than as nothing to
///    say yet.
///  - ``asking`` is the seconds between the press and the router's answer. The
///    panel polls on its own cadence, so without this state a press reads as a
///    switch that did nothing for up to half a minute.
///  - ``reachable`` names the address and the port, and it EXPIRES: a mapping
///    is granted for a lifetime, and a line that outlives it would show an
///    address that has stopped working. Past that instant this state is not
///    produced at all (``state(on:reading:now:)``), so a stale address cannot
///    be drawn even by a caller that kept an old reading.
///  - ``routerSilent`` is the ordinary outcome on most home and office
///    routers, and it is amber rather than red: nothing failed, a request just
///    has nowhere to land yet.
///  - ``retrying`` is ``asking``'s twin, reached from the row's own button
///    instead of the switch, so a caller can tell the two presses apart
///    without a second field.
public enum PeerInternetReach: Equatable, Sendable {
    case off
    case asking
    case reachable(address: String, port: Int, expires: Date)
    case routerSilent
    /// `tcr` itself refused, or answered something this build could not read.
    /// Its own words, unparaphrased: a probe that failed is not a router that
    /// said no, and collapsing the two would hide a broken `tcr` behind a
    /// sentence about somebody's router.
    case unreadable(String)
    /// The row's own "Ask the router again" button was pressed, and the
    /// answer has not arrived. A typed case rather than a bool sitting beside
    /// the state, so the button's "Asking…" and its disabled state read off
    /// this one value, the same way every other line on the row does.
    case retrying

    /// Which state the switch and the last reading put this Mac in.
    ///
    /// `nil` reading with the switch ON is ``asking``: the press has happened
    /// and the answer has not arrived. It is never ``routerSilent``, because
    /// "we have not heard yet" and "the router said no" are different facts
    /// and only one of them is a finding. `retrying` is the same absence, but
    /// reached from the row's own retry button rather than from turning the
    /// switch on, so the caller says which by passing `retrying: true`.
    public static func state(
        on: Bool, reading: PeerReachReading?, retrying: Bool = false, now: Date
    )
        -> PeerInternetReach
    {
        guard on else { return .off }
        guard let reading else { return retrying ? .retrying : .asking }
        // `heldMapping`, not `mapping`: `mapping` is this one call's own
        // probe outcome, but `heldMapping.expiresAtMs` is the absolute
        // instant the SERVING process's keeper recorded, so it needs no
        // reconstruction against `readAt` and cannot drift from it.
        guard let held = reading.heldMapping else { return .routerSilent }
        // Absent rather than a guess, same rule `MappingRecord::external_
        // address` states: a mapped port with no address is not something a
        // peer across the internet can dial.
        guard let address = held.externalAddress else { return .routerSilent }
        // The lead's ruling, in one comparison: when the held mapping's
        // deadline has passed, the line reads "Router did not answer" again
        // rather than an address nothing is holding open.
        guard now < held.expires else { return .routerSilent }
        // The EXTERNAL port, which is the one a Mac across the internet
        // dials. `listenPort`/`held.internalPort` is what this Mac binds
        // locally and the two are equal only because `run_peer_reach` asks
        // for that pairing; printing the local one would name a port nobody
        // outside can reach.
        return .reachable(address: address, port: held.externalPort, expires: held.expires)
    }

    /// The line under the switch, or `nil` when the state draws none.
    ///
    /// No jargon: the words a router speaks are the router's business, and
    /// nothing here names a protocol.
    public var line: String? {
        switch self {
        case .off:
            return nil
        case .asking, .retrying:
            return "Asking your router for a way in. This takes a few seconds."
        case .reachable(let address, let port, _):
            return "Reachable at \(address):\(port). Your router agreed to hold this port "
                + "open; it is renewed automatically and closes the moment this switch "
                + "goes off."
        case .routerSilent:
            return "Router did not answer. No path is open from outside right now. A Mac "
                + "you trust may still reach this one by forwarding through a third Mac "
                + "you both trust."
        case .unreadable(let message):
            return "Could not ask your router: \(message)"
        }
    }

    /// Whether the line is drawn in the amber the mockup gives it. Only the
    /// miss is amber; asking is ordinary text, because waiting is not a
    /// finding.
    public var isWarning: Bool {
        switch self {
        case .routerSilent, .unreadable: return true
        case .off, .asking, .retrying, .reachable: return false
        }
    }

    /// Whether this is one of the two states that ended without a path, the
    /// only ones the row's "Ask the router again" button appears on.
    public var canRetry: Bool {
        switch self {
        case .routerSilent, .unreadable: return true
        case .off, .asking, .retrying, .reachable: return false
        }
    }

    /// The row's own sub-line, which says what each side of the switch means
    /// in the words the mockup uses.
    public static func rowDetail(on: Bool) -> String {
        on
            ? "On: a trusted Mac can still find this one after it leaves this network."
            : "Off: this Mac answers only Macs on this network, or a Mac that already has "
                + "your last known address."
    }
}

extension PeerCommand {
    /// `tcr peer internet on|off`: the state the switch MOVES to, the same
    /// rule every other switch on these surfaces keeps.
    public static func internet(on: Bool) -> [String] {
        ["peer", "internet", on ? "on" : "off"]
    }

    /// `tcr peer reach --map --json`: the readout under the switch.
    ///
    /// **Run on the PRESS, never on the poll**, and the reason is in
    /// `main.rs`'s own doc on the flag: `--map` writes to the router, and a
    /// repeated request replaces an existing mapping's lifetime rather than
    /// adding one (RFC 6886 s3.3), so a panel probing every 30 s would cut
    /// whatever the proxy holds down to this probe's two minutes, over and
    /// over. Without `--map` the verb reports `not asked for` and cannot see
    /// the running proxy's mapping at all: there is
    /// no read-only view of the mapping in `tcr` today.
    public static let reach = ["peer", "reach", "--map", "--json"]
}
