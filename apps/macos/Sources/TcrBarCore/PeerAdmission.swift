import Foundation

// Decision row 10's two-phase pairing and row 11's caps, as values the panel
// can draw and a test can build.
//
// The field names are `src/main.rs:1129-1210`'s (`PeerLsJson`, which is
// `camelCase`) over `src/peer/state.rs`'s `Knock`, `Mute`, `Ban` and
// `BanReason`. Nothing here parses a key or an id: the panel shows what it was
// given and hands the same string back as argv, which is why `instanceId` is a
// `String` and not a decoded eight bytes.

/// A Mac that knocked and is waiting for Accept.
///
/// **A knock is a request, never a trust decision** (decision row 10). It
/// reveals no static key, so there is nothing to pin and nothing to compare
/// yet: the row exists so an operator can say yes, and Accept is what opens the
/// 120-second window in which the six digits appear.
public struct PeerKnock: Decodable, Equatable, Identifiable, Sendable {
    /// Where to ANSWER it: `host:port` when the knock said which port its
    /// listener is bound to, the host alone when it did not.
    ///
    /// **This is the string to dial, and the whole reason it is not the bare
    /// host.** A knock arrives on a connection whose source port is ephemeral,
    /// so the Rust side coalesces its queue on the bare IP and hands this
    /// surface the port the knocker named beside it. Pass it to
    /// ``PeerCommand/pair(address:)`` unchanged: `tcr peer pair` fills the
    /// default port in for a bare host, and a Mac listening anywhere else can
    /// be reached only through the port in this string.
    ///
    /// There is no separate port property here on purpose. The port arrives on
    /// its own key too, and a second field would be a second way to build the
    /// same dial string; this panel shows what it was given and hands the same
    /// string back as argv.
    public var addr: String
    /// The ephemeral, per-boot id it knocked under, 16 lower-case hex
    /// characters. **This is not identity**, it is what `accept`, `ignore`
    /// and `block` take as their argument, because the name in a knock is only
    /// proposed and two knocks can propose the same one.
    public var instanceId: String
    /// The name it asked to be shown as, already sanitized twice (on arrival,
    /// and again on the way out of `tcr peer ls --json`). `nil` shows the
    /// address, which is also what a Mac with `announceName` off looks like.
    public var proposedName: String?
    /// The wire version it claimed.
    public var wireVersion: Int
    public var firstSeenMs: Int64
    public var lastSeenMs: Int64

    /// The row's list identity: the instance id, which is also the argument
    /// every verb on the row takes. One value, so a row cannot be drawn under
    /// one key and acted on under another.
    public var id: String { instanceId }

    public init(
        addr: String, instanceId: String, proposedName: String? = nil, wireVersion: Int = 1,
        firstSeenMs: Int64 = 0, lastSeenMs: Int64 = 0
    ) {
        self.addr = addr
        self.instanceId = instanceId
        self.proposedName = proposedName
        self.wireVersion = wireVersion
        self.firstSeenMs = firstSeenMs
        self.lastSeenMs = lastSeenMs
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: Keys.self)
        self.addr = try c.decodeIfPresent(String.self, forKey: .addr) ?? ""
        self.instanceId = try c.decodeIfPresent(String.self, forKey: .instanceId) ?? ""
        self.proposedName = try c.decodeIfPresent(String.self, forKey: .proposedName)
        self.wireVersion = try c.decodeIfPresent(Int.self, forKey: .wireVersion) ?? 0
        self.firstSeenMs = try c.decodeIfPresent(Int64.self, forKey: .firstSeenMs) ?? 0
        self.lastSeenMs = try c.decodeIfPresent(Int64.self, forKey: .lastSeenMs) ?? 0
    }

    private enum Keys: String, CodingKey {
        case addr, instanceId, proposedName, wireVersion, firstSeenMs, lastSeenMs
    }
}

/// An address the operator pressed Ignore on. A mute lifts by itself.
public struct PeerMute: Decodable, Equatable, Identifiable, Sendable {
    public var addr: String
    /// Absolute deadline, Unix milliseconds.
    public var untilMs: Int64

    public var id: String { addr }

    public init(addr: String, untilMs: Int64) {
        self.addr = addr
        self.untilMs = untilMs
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: Keys.self)
        self.addr = try c.decodeIfPresent(String.self, forKey: .addr) ?? ""
        self.untilMs = try c.decodeIfPresent(Int64.self, forKey: .untilMs) ?? 0
    }

    private enum Keys: String, CodingKey { case addr, untilMs }
}

/// An address, and its static key, when a handshake got far enough to learn
/// one, the operator pressed Block on. A ban does NOT lift by itself.
public struct PeerBan: Decodable, Equatable, Identifiable, Sendable {
    public var addr: String
    /// The static key, when one was learned. `nil` on a ban that came from a
    /// knock, which reveals no static key at all.
    public var key: String?
    public var sinceMs: Int64
    public var reason: PeerBanReason

    public var id: String { addr }

    public init(
        addr: String, key: String? = nil, sinceMs: Int64 = 0,
        reason: PeerBanReason = .blocked
    ) {
        self.addr = addr
        self.key = key
        self.sinceMs = sinceMs
        self.reason = reason
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: Keys.self)
        self.addr = try c.decodeIfPresent(String.self, forKey: .addr) ?? ""
        self.key = try c.decodeIfPresent(String.self, forKey: .key)
        self.sinceMs = try c.decodeIfPresent(Int64.self, forKey: .sinceMs) ?? 0
        // An unknown reason decodes to `.unknown` rather than failing the whole
        // document: a newer `tcr` that adds a third reason must not blank the
        // Blocked list on an older panel, and "blocked, and this build cannot
        // say why" is the honest line for it.
        self.reason = try c.decodeIfPresent(PeerBanReason.self, forKey: .reason) ?? .unknown
    }

    private enum Keys: String, CodingKey { case addr, key, sinceMs, reason }
}

/// Why an address is blocked. `src/peer/state.rs:177`'s enum, kebab-case on the
/// wire, as a typed value rather than free text so the panel row and the JSON
/// field cannot disagree.
public enum PeerBanReason: String, Decodable, Equatable, Sendable {
    case blocked
    case forgottenAndBlocked = "forgotten-and-blocked"
    /// A reason this build does not know. Decoded rather than thrown, for the
    /// reason ``PeerBan/init(from:)`` gives.
    case unknown

    public init(from decoder: Decoder) throws {
        let raw = try decoder.singleValueContainer().decode(String.self)
        self = PeerBanReason(rawValue: raw) ?? .unknown
    }

    /// What the Blocked row says after the address. The operator's words, not
    /// the enum's: `forgotten-and-blocked` is two acts and the row says both.
    public var sentence: String {
        switch self {
        case .blocked:
            return "you pressed Block on its request to connect"
        case .forgottenAndBlocked:
            return "you stopped trusting it and blocked it in one act"
        case .unknown:
            return "blocked, and this build of TcrBar cannot say why"
        }
    }
}

/// The caps decision row 11 shipped, read off `tcr peer ls --json` rather than
/// restated here.
///
/// **The numbers are the binary's, never this panel's.** Every one of them is a
/// constant in `src/peer/{discovery,listener,state}.rs` and the pane used to
/// print six literals beside them, which is the same figure in two places, and
/// the copy that drifts is the one nobody runs. A `tcr` too old to send `caps`
/// gets "not read yet" on those rows, which is the honest absence rather than a
/// number nobody enforces.
public struct PeerCaps: Decodable, Equatable, Sendable {
    /// How many found rows are shown at all.
    public var foundRows: Int
    /// How many of those may come from one address.
    public var foundPerAddress: Int
    /// How many outstanding knocks this Mac holds.
    public var pending: Int
    /// The knock rate limit's interval, in milliseconds.
    public var knockIntervalMs: Int64
    /// How many knocks may arrive back-to-back before the interval bites.
    public var knockBurst: Int
    /// Sockets accepted from Macs that have not authenticated.
    public var unauthenticatedSockets: Int

    public init(
        foundRows: Int, foundPerAddress: Int, pending: Int, knockIntervalMs: Int64,
        knockBurst: Int, unauthenticatedSockets: Int
    ) {
        self.foundRows = foundRows
        self.foundPerAddress = foundPerAddress
        self.pending = pending
        self.knockIntervalMs = knockIntervalMs
        self.knockBurst = knockBurst
        self.unauthenticatedSockets = unauthenticatedSockets
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: Keys.self)
        self.foundRows = try c.decodeIfPresent(Int.self, forKey: .foundRows) ?? 0
        self.foundPerAddress = try c.decodeIfPresent(Int.self, forKey: .foundPerAddress) ?? 0
        self.pending = try c.decodeIfPresent(Int.self, forKey: .pending) ?? 0
        self.knockIntervalMs = try c.decodeIfPresent(Int64.self, forKey: .knockIntervalMs) ?? 0
        self.knockBurst = try c.decodeIfPresent(Int.self, forKey: .knockBurst) ?? 0
        self.unauthenticatedSockets =
            try c.decodeIfPresent(Int.self, forKey: .unauthenticatedSockets) ?? 0
    }

    private enum Keys: String, CodingKey {
        case foundRows, foundPerAddress, pending, knockIntervalMs, knockBurst
        case unauthenticatedSockets
    }

    /// One Advanced row per cap: its label, and the value in the operator's
    /// words. Derived, so the pane draws whatever the binary sent and cannot
    /// hard-code a seventh number beside them.
    public var advancedRows: [(label: String, value: String)] {
        [
            ("Macs shown at once", "\(foundRows)"),
            ("From one address", "\(foundPerAddress)"),
            ("Pairing requests held", "\(pending)"),
            ("Knocks accepted", knockRate),
            ("Unauthenticated sockets", "\(unauthenticatedSockets)"),
        ]
    }

    /// `1 every 10 s, burst 3`. The interval arrives in milliseconds and is
    /// said in seconds, because the cap an operator reasons about is "how
    /// often" and 10000 is not that sentence.
    public var knockRate: String {
        let seconds = Double(knockIntervalMs) / 1000
        let interval =
            seconds >= 1
            ? "\(seconds == seconds.rounded() ? String(Int(seconds)) : String(format: "%.1f", seconds)) s"
            : "\(knockIntervalMs) ms"
        return "1 every \(interval), burst \(knockBurst)"
    }
}

/// The words the admission surfaces use, in one place so the tab and the pane
/// cannot phrase the same request two ways.
public enum PeerAdmission {
    /// `loft-mini (10.0.1.24) wants to connect`, or the address alone when no
    /// name was proposed.
    ///
    /// The name is in the sentence and the address is always beside it: a
    /// proposed name is a string a stranger on this network chose, so the
    /// address is the part the operator can actually check.
    ///
    /// **`connect`, never `pair`.** Three words were used for one act on one
    /// screen: the found row's button said Trust, this card said Accept, and
    /// this headline said `wants to pair`. `pair` is the name of a CLI
    /// subcommand, it is the first word a stranger's Mac says to somebody who
    /// has never read the CLI, and the state it leads to is spelled `trusted`
    /// everywhere else. One act, one vocabulary: a Mac wants to connect,
    /// Accept opens the six-digit compare, and Trust is the last press.
    public static func knockTitle(_ knock: PeerKnock) -> String {
        guard let name = knock.proposedName, !name.isEmpty else {
            return "\(knock.addr) wants to connect"
        }
        return "\(name) (\(knock.addr)) wants to connect"
    }

    /// ``knockTitle`` without the address folded in: `loft-mini wants to
    /// connect`, or the address alone when no name was proposed. Every
    /// renderer of the knock CARD (as opposed to a spoken hint, where one line
    /// is fine) draws this beside ``knockAddressLine`` rather than
    /// ``knockTitle`` directly, because a one-line control truncating
    /// `"loft-mini (10.0.1.24) wants to connect"` cuts the address mid-digit,
    /// which is the one part of the sentence an operator is meant to check.
    public static func knockNameLine(_ knock: PeerKnock) -> String {
        guard let name = knock.proposedName, !name.isEmpty else {
            return "\(knock.addr) wants to connect"
        }
        return "\(name) wants to connect"
    }

    /// The address on its own line beside ``knockNameLine``: `10.0.1.24`, or
    /// `no name sent` when the knock proposed none.
    ///
    /// **The deadline is no longer folded in here.** It is counted by
    /// ``knockExpiryPill(_:now:)`` and drawn as a pill on this same row, and a
    /// countdown printed in both places is one fact said twice, which is the
    /// one that drifts. Nothing in this line counts any more, so it takes no
    /// clock.
    ///
    /// Never `nil`. When no name was proposed ``knockNameLine`` already IS the
    /// address, so this line says what is MISSING rather than repeating the
    /// address or collapsing to nothing: a card whose second line vanished
    /// looked like a card whose second line failed to load.
    public static func knockAddressLine(_ knock: PeerKnock) -> String {
        let named = !(knock.proposedName ?? "").isEmpty
        return named ? knock.addr : "no name sent"
    }

    /// `Expires in 9m` for the pill on the knock card's address row, or `nil`
    /// when there is nothing honest to count.
    ///
    /// Over ``knockExpiry(firstSeenMs:now:)`` rather than beside it: that
    /// function already decides both absences, a knock past its deadline and
    /// one whose sender stamped no time, and `nil` here is the honest answer
    /// for each. No pill at all, never a zero and never an invented ten
    /// minutes. The card keeps its accent either way: it still expires, this
    /// Mac just cannot say when.
    public static func knockExpiryPill(_ knock: PeerKnock, now: Date) -> String? {
        guard let remaining = knockExpiry(firstSeenMs: knock.firstSeenMs, now: now) else {
            return nil
        }
        return "Expires in \(remaining)"
    }

    /// How long a knock stands before the Mac holding it drops the row.
    ///
    /// The Rust side owns this number: `KNOCK_TTL_MS` in `src/peer/state.rs`
    /// is what drops an unanswered knock, and `PAIR_WAIT` in
    /// `src/peer/pair.rs` is the matching wait on the Mac that sent it.
    /// Nothing on the wire reports either, so this is a copy of a constant
    /// rather than a reading, and it is the only copy: every surface counts
    /// against this and none of them describes it in prose.
    public static let knockExpirySeconds: TimeInterval = 600

    /// `7m` left on a knock, from the timestamp the knock itself carries, or
    /// `nil` when there is nothing honest to count.
    ///
    /// `nil` covers both a knock past its deadline and one whose producer
    /// sent no timestamp at all, which arrives as `0` and would otherwise
    /// count from 1970. Neither gets an invented figure.
    public static func knockExpiry(firstSeenMs: Int64, now: Date) -> String? {
        guard firstSeenMs > 0 else { return nil }
        let age = now.timeIntervalSince1970 - Double(firstSeenMs) / 1000
        let remaining = knockExpirySeconds - age
        guard remaining > 0 else { return nil }
        return PeerFormat.span(remaining)
    }

    /// What Accept buys, under the address. Decision row 10 in one sentence:
    /// approval comes first and nothing is shared until Trust.
    ///
    /// **One sentence, because the card used to carry two.** This line and the
    /// block under the buttons both said "six digits" and both said "nothing
    /// is shared until Trust", which is one promise written twice on one card;
    /// the block is gone and what it added, that this Mac pins, carries and
    /// serves nothing meanwhile, is the second half of this sentence. The card
    /// ends up shorter than it was, which is what pays for a line that wraps
    /// whole instead of being cut mid-word.
    public static let knockDetail =
        "Accepting shows six digits on both screens. Until you press Trust on both, this Mac "
        + "pins nothing, carries nothing and serves nothing."

    /// `12 shown, 3 more not shown`, or `nil` when nothing was held back.
    ///
    /// Both numbers, because either alone is a different question: the cap is
    /// 12 found rows and 2 per address (decision row 11), and a list that
    /// silently stopped at 12 looks exactly like a network with 12 Macs on it.
    public static func limitedFooter(shown: Int, limited: Int) -> String? {
        guard limited > 0 else { return nil }
        let more = limited == 1 ? "1 more not shown" : "\(limited) more not shown"
        return "\(shown) shown, \(more)"
    }

    /// `quiet for another 47 min`, or `the hour is up` once the deadline has
    /// passed. A muted address is a row an operator may want to un-mute by
    /// hand, so the row says when it lifts rather than only that it is muted.
    public static func muteSentence(_ mute: PeerMute, now: Date) -> String {
        let remaining = Double(mute.untilMs) / 1000 - now.timeIntervalSince1970
        guard remaining > 0 else { return "the hour is up; it may knock again" }
        if remaining < 60 { return "quiet for another \(Int(remaining)) s" }
        return "quiet for another \(Int((remaining / 60).rounded())) min"
    }

    // MARK: The knock this Mac SENT
    //
    // Decision row 10 has two directions and the tab drew only one. A knock
    // arriving is `knockTitle`/`knockDetail` above; a knock this Mac sent by
    // pressing Trust had no words at all, and no row state either, so the row
    // was byte-identical before and after the press.
    //
    // Both strings below are built from ONE fragment. The row wants a
    // lower-case sub-line beside its other fragments ("found 2s ago · not
    // trusted") and the sheet wants a sentence; two literals would be two
    // spellings of one fact, and the copy that drifts is the one nobody reads.

    /// `waiting for studio-mac to accept`, the found row's sub-line once its
    /// knock is away.
    ///
    /// No promise of digits in it. Row 10: the six digits appear only after
    /// somebody on the other Mac presses Accept, and until then there is
    /// nothing to compare.
    public static func waitingLine(name: String) -> String {
        "waiting for \(name) to accept"
    }

    /// The same fact as a sentence, for the sheet's own title.
    public static func waitingTitle(name: String) -> String {
        sentence(waitingLine(name: name))
    }

    /// What the waiting row and the waiting sheet both say happens next.
    ///
    /// The deadline is NOT in it. It used to close with "a request nobody
    /// answers expires in ten minutes", a figure described on a surface that
    /// knows exactly when this panel sent the request;
    /// ``waitingSentence(expiresIn:)`` counts it instead.
    public static let waitingSentence =
        "The request is on that Mac now. Nothing is pinned, carried or served until somebody "
        + "there accepts it; six digits appear on both screens then, and not before."

    /// The same sentence with the deadline counted: `… This request expires
    /// in 9m.`
    ///
    /// `remaining` is seconds left, counted by the caller from when it sent
    /// the request against ``knockExpirySeconds``. `nil`, or a deadline
    /// already past, adds nothing at all rather than a zero or a negative
    /// span: a sheet with nothing to count says nothing.
    public static func waitingSentence(expiresIn remaining: TimeInterval?) -> String {
        guard let remaining, remaining > 0 else { return waitingSentence }
        return waitingSentence + " This request expires in \(PeerFormat.span(remaining))."
    }

    /// Cancel, on a waiting row. It stops this panel waiting and claims
    /// nothing about the other Mac: there is no verb that withdraws a knock,
    /// and saying so is cheaper than a button that pretends to.
    public static let cancelWaitingHelp =
        "Stops waiting here. The request stands on that Mac until somebody answers it or it "
        + "expires."

    /// One fragment as a sentence: the first character upper-cased, the rest
    /// untouched. Never `capitalized`, which would also re-case `studio-mac`.
    static func sentence(_ fragment: String) -> String {
        guard let first = fragment.first else { return fragment }
        return first.uppercased() + fragment.dropFirst()
    }

    /// `blocked 3 h ago · you pressed Block on its pairing request`, plus
    /// `and its key` when a static key was learned, which is the field that
    /// makes the block survive a new address.
    public static func blockSentence(_ ban: PeerBan, now: Date) -> String {
        let age = max(0, now.timeIntervalSince1970 - Double(ban.sinceMs) / 1000)
        var sentence = "blocked \(PeerFormat.span(age)) ago · \(ban.reason.sentence)"
        if ban.key != nil {
            sentence += ", and its key too, so a new address does not get it back in"
        }
        return sentence
    }
}

extension PeerCommand {
    // MARK: Decision row 10's verbs
    //
    // Each takes the INSTANCE ID, not a name: `src/main.rs:201-207`'s
    // `PeerDecideArgs.target` accepts the instance id or the address, and the
    // proposed name is neither. Two knocks can propose one name.

    /// `tcr peer accept <instance>`, opens a 120 s window for that one Mac
    /// and nothing else. The six digits come next, in the handshake.
    public static func accept(instance: String) -> [String] { ["peer", "accept", instance] }

    /// `tcr peer ignore <instance>`, the row goes and that address is muted
    /// for an hour. A knock nobody answers expires by itself in ten minutes,
    /// so doing nothing is an answer too.
    public static func ignore(instance: String) -> [String] { ["peer", "ignore", instance] }

    /// `tcr peer block <instance>`, the address, and the key too when one was
    /// learned. Decision row 11's "ban scope: both".
    public static func block(instance: String) -> [String] { ["peer", "block", instance] }

    /// `tcr peer block <addr>`, the same verb, aimed at a row that is not a
    /// knock.
    ///
    /// `src/main.rs:194-207`'s `PeerDecideArgs.target` is ONE shape for
    /// `accept`, `ignore` and `block`, and it accepts an instance id or an
    /// address: the handler looks the target up in the pending queue and falls
    /// back to treating it as an address (`src/main.rs:2057-2060`). So this
    /// needs no CLI change, and it is a second factory rather than a rename of
    /// ``block(instance:)`` so each call site names the thing it is actually
    /// holding. A found row has an address and no id at all (decision row 9),
    /// and a trusted row's id is not something `block` takes.
    ///
    /// The reachability this closes: Block had exactly two call sites and both
    /// were a knock row, so a Mac that floods the found list, or one whose
    /// knock already expired, could not be banned at any click depth.
    public static func block(address: String) -> [String] { ["peer", "block", address] }

    /// `tcr peer unblock <addr>`, the Blocked list's own control.
    /// `src/main.rs:210-217`: the argument is the ADDRESS, as `tcr peer ls
    /// --json` lists it under `blocked`, and not the instance id the other
    /// three take.
    public static func unblock(address: String) -> [String] { ["peer", "unblock", address] }

    /// `tcr peer pending --json`. The tab reads the pending rows out of
    /// `tcr peer ls --json` instead, in ONE read: two calls would see two
    /// different instants and the footer would be able to disagree with the
    /// rows above it. This is here for a caller that wants only the queue.
    public static let pending = ["peer", "pending", "--json"]
}
