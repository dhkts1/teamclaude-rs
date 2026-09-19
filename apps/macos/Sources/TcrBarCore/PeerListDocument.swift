import Foundation

// Moved out of `TcrBar/PanelV4/PeersTabV4.swift`: the document is pure
// `Decodable` data with no SwiftUI in it, and while it sat in
// the executable target every assertion about it had to read the SOURCE as
// text (`Package.swift:39-43` gives the test target `TcrBarCore` alone). The
// four blocks decision rows 10 to 13 added, `pending`, `blocked`, `muted`,
// `caps` and per-account `lentTo`, are the ones a grep is least able to
// check: each is a nested shape whose field names have to match what
// `src/main.rs`'s `PeerLsJson` writes, and the only honest gate for that is
// decoding the real JSON.

/// The document `tcr peer ls --json` answers with, a SIBLING document, not an
/// entry in the status array.
///
/// That split is the reason this type exists at all rather than a field on
/// `Fleet`: peers are read with their own subcommand, on their own cadence, and
/// an older `tcr` answers `{"supported": false}` to it while answering
/// `tcr status --json` perfectly. Folding the two together would make a missing
/// peer subcommand look like an unreadable fleet, which is the one outcome
/// forbidden by name.
///
/// Decoded permissively on purpose: every field is optional or defaulted, so a
/// row that arrives with one more key than this build knows about still draws,
/// and a row missing a key this build expects draws as the honest absence
/// rather than refusing the whole document. That tolerance is load-bearing
/// right now, `src/main.rs:1129-1160` writes `supported`, `peers`, the four
/// admission blocks and `caps` and writes NONE of `finding`, `sharing`,
/// `answeringOn` or the six Settings readouts, so a panel that required them
/// would collapse against the `tcr` in this very tree.
public struct PeerListDocument: Decodable, Equatable, Sendable {
    /// `false` from a `tcr` with no peer subcommand at all. The tab collapses
    /// to one line and says so.
    public var supported: Bool
    /// Whether discovery is up. A separate field from `peers` being empty,
    /// because "looking, and there may be nothing to find" and "not looking"
    /// are different states and the panel must never guess which it is in.
    public var finding: Bool
    /// Whether `peer.shareAccounts` is on.
    public var sharing: Bool
    public var peers: [PeerEntry]
    /// The one line an operator reads first when their own accounts are dry:
    /// which Mac is answering for them right now.
    public var answeringOn: AnsweringOn?

    // Settings > Peers reads the six below and the Peers tab reads none of
    // them, they are the detail one level down. Every one is OPTIONAL and
    // renders as "not read yet" when
    // absent, never as a plausible-looking default: a pane that invented a
    // node id would be showing a person a key to compare against nothing.
    /// This Mac's display name (`peer.name`, default the computer's own).
    public var name: String?
    /// Whether the beacon carries that name (`peer.announceName`).
    public var announceName: Bool?
    /// This Mac's own peer id, as `tcr peer id` prints it. Learned inside the
    /// handshake by a peer and never announced (decision row 9).
    public var nodeId: String?
    /// Where the peer listener is bound, or absent when it is down.
    public var listenAddress: String?
    /// `auto`, `none`, or a peer's name.
    public var via: String?
    /// `maxHops`. 1 in the minimum; a hop above 1 is a setting.
    public var maxHops: Int?
    /// Whether this Mac asks its router to let a pinned Mac reach it from off
    /// its own network (`peer.internet`).
    ///
    /// Optional like the rest of this block and for the same reason: the `tcr`
    /// in this tree writes none of these keys, so absent is "not read yet" and
    /// the switch draws the default OFF rather than a state nobody reported.
    public var internet: Bool?

    // MARK: Decision row 10's admission blocks
    //
    // Counts AND rows, both, for the reason `src/main.rs:1133-1141` gives: a
    // count alone makes the panel ask a second question to render the row, and
    // the two calls would see two different instants. The panel therefore
    // NEVER derives a count from `rows.count` where the producer sent one:
    // `pendingCount` is the number the footer says and `pending` is what it
    // draws, and when the producer holds rows back those two disagree on
    // purpose.

    /// Macs that knocked and are waiting for Accept. Decision row 10: nothing
    /// is pinned, carried or served until the operator answers one of these.
    public var pending: [PeerKnock]
    /// `pending.count` as the producer counted it.
    public var pendingCount: Int
    /// Addresses (and keys, where a handshake got far enough to learn one)
    /// the operator pressed Block on. A ban does not lift by itself.
    public var blocked: [PeerBan]
    public var blockedCount: Int
    /// Addresses the operator pressed Ignore on. A mute DOES lift by itself.
    public var muted: [PeerMute]
    public var mutedCount: Int
    /// How many found rows this Mac is holding back, the "N more not shown"
    /// footer's number. Decision row 11 caps found rows at 12 and 2 per
    /// address, and a cap that silently drops rows is a cap an operator cannot
    /// tell from an empty network.
    public var limited: Int
    /// The caps themselves, so the Advanced pane and the binary cannot
    /// disagree about what they are. `nil` from a `tcr` that predates them,
    /// which is the one case the pane is allowed to say "not read yet" for
    /// rather than print a number nobody enforced.
    public var caps: PeerCaps?

    /// Who is drawing on each of this Mac's accounts, keyed by the SANITIZED
    /// account label `tcr status` prints, never an email and never a UUID,
    /// because this repository is public and a screenshot of the account card
    /// would carry it.
    ///
    /// Absent from
    /// the `tcr` in this tree today, so it decodes to an empty map and an account
    /// card draws no line at all, which is the state the mockup's scene 64
    /// asks for when an account is inside no lease.
    public var lentTo: [String: [PeerLentToEntry]]

    /// Where each account's requests leave from, keyed by the SANITIZED
    /// account label, the same key ``lentTo`` uses and never an email.
    ///
    /// Empty from every `tcr` in this tree: nothing writes the account's
    /// `egress` and `egressStrict` onto any read yet, so an account draws NO
    /// "Exits from" row at all rather than a picker reading "This Mac" for an
    /// account that may be pinned to a peer. That absence is the honest state
    /// until `tcr` writes the field.
    public var exits: [String: AccountExit]

    /// Whether the LIVE half of the read answered, for the one row that has
    /// to tell "this Mac looked and found no way there" from "nothing
    /// looked".
    ///
    /// Not a wire key and never decoded: ``mergingLive(_:)`` is its only
    /// writer. `nil` means no live read was folded in at all, which is every
    /// document built by hand, and those keep the measured wording rather
    /// than reporting an absence nobody observed.
    ///
    /// It is HERE and not on a row because that is where the fact is. A row's
    /// `paths` array is `[]` in both cases, deliberately (see
    /// ``PeerEntry/paths``, whose own note says the two readings were the
    /// same sentence), and no per-row key distinguishes them; what does is
    /// whether `tcr peer status --json` answered at all, which is one fact
    /// about one read.
    public var liveAnswered: Bool?

    enum CodingKeys: String, CodingKey {
        case supported, finding, sharing, peers, answeringOn
        case name, announceName, nodeId, listenAddress, via, maxHops, internet
        case pending, pendingCount, blocked, blockedCount, muted, mutedCount, limited, caps
        case lentTo, exits
    }

    public init(
        supported: Bool = true, finding: Bool = false, sharing: Bool = false,
        peers: [PeerEntry] = [], answeringOn: AnsweringOn? = nil,
        name: String? = nil, announceName: Bool? = nil, nodeId: String? = nil,
        listenAddress: String? = nil, via: String? = nil, maxHops: Int? = nil,
        internet: Bool? = nil,
        pending: [PeerKnock] = [], pendingCount: Int? = nil,
        blocked: [PeerBan] = [], blockedCount: Int? = nil,
        muted: [PeerMute] = [], mutedCount: Int? = nil,
        limited: Int = 0, caps: PeerCaps? = nil,
        lentTo: [String: [PeerLentToEntry]] = [:],
        exits: [String: AccountExit] = [:]
    ) {
        self.supported = supported
        self.finding = finding
        self.sharing = sharing
        self.peers = peers
        self.answeringOn = answeringOn
        self.name = name
        self.announceName = announceName
        self.nodeId = nodeId
        self.listenAddress = listenAddress
        self.via = via
        self.maxHops = maxHops
        self.internet = internet
        self.pending = pending
        self.pendingCount = pendingCount ?? pending.count
        self.blocked = blocked
        self.blockedCount = blockedCount ?? blocked.count
        self.muted = muted
        self.mutedCount = mutedCount ?? muted.count
        self.limited = limited
        self.caps = caps
        self.lentTo = lentTo
        self.exits = exits
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        // `supported` defaults TRUE when the key is absent: a `tcr` new enough
        // to have the subcommand answers rows without restating it, and
        // defaulting false would collapse a working tab.
        self.supported = try c.decodeIfPresent(Bool.self, forKey: .supported) ?? true
        self.finding = try c.decodeIfPresent(Bool.self, forKey: .finding) ?? false
        self.sharing = try c.decodeIfPresent(Bool.self, forKey: .sharing) ?? false
        self.peers = try c.decodeIfPresent([PeerEntry].self, forKey: .peers) ?? []
        self.answeringOn = try c.decodeIfPresent(AnsweringOn.self, forKey: .answeringOn)
        self.name = try c.decodeIfPresent(String.self, forKey: .name)
        self.announceName = try c.decodeIfPresent(Bool.self, forKey: .announceName)
        self.nodeId = try c.decodeIfPresent(String.self, forKey: .nodeId)
        self.listenAddress = try c.decodeIfPresent(String.self, forKey: .listenAddress)
        self.via = try c.decodeIfPresent(String.self, forKey: .via)
        self.maxHops = try c.decodeIfPresent(Int.self, forKey: .maxHops)
        self.internet = try c.decodeIfPresent(Bool.self, forKey: .internet)
        self.pending = try c.decodeIfPresent([PeerKnock].self, forKey: .pending) ?? []
        self.blocked = try c.decodeIfPresent([PeerBan].self, forKey: .blocked) ?? []
        self.muted = try c.decodeIfPresent([PeerMute].self, forKey: .muted) ?? []
        // A count the producer sent WINS over the row count, and falls back to
        // it only when the key is absent. The other order would quietly turn
        // "8 held, 8 shown" into a footer that can never say anything.
        self.pendingCount =
            try c.decodeIfPresent(Int.self, forKey: .pendingCount) ?? self.pending.count
        self.blockedCount =
            try c.decodeIfPresent(Int.self, forKey: .blockedCount) ?? self.blocked.count
        self.mutedCount =
            try c.decodeIfPresent(Int.self, forKey: .mutedCount) ?? self.muted.count
        self.limited = try c.decodeIfPresent(Int.self, forKey: .limited) ?? 0
        self.caps = try c.decodeIfPresent(PeerCaps.self, forKey: .caps)
        self.lentTo =
            try c.decodeIfPresent([String: [PeerLentToEntry]].self, forKey: .lentTo) ?? [:]
        self.exits =
            try c.decodeIfPresent([String: AccountExit].self, forKey: .exits) ?? [:]
    }

    /// One Mac, as the wire describes it.
    ///
    /// **No key and no id on an untrusted row, by design.** Decision row 9:
    /// the beacon carries presence, a port, and the name when announcing is
    /// on, and nothing else, identity is learned inside the handshake after
    /// Trust. So `id` is optional and a found-not-trusted row has none, which
    /// is why the panel cannot draw a fingerprint for a Mac it has not
    /// trusted yet.
    public struct PeerEntry: Decodable, Equatable, Sendable {
        public var id: String?
        /// The short `tcr-…` form of ``id``, from `status.rs`'s own `display`
        /// key.
        ///
        /// Before this key existed, `PeerStatusRow::id` (`src/status.rs`) WAS
        /// the short form, which is what every live row's `id` decoded to;
        /// `tcr peer ls --json`'s rows carried the 52-character WIRE form
        /// under `node`/`id`, so ``mergingLive(_:)`` joined the two documents
        /// on two different spellings of the same Mac and never matched
        /// anything. Now `id` is the wire form on both documents (the join
        /// key) and `display` is the short form an operator reads, kept
        /// separately so a panel never has to guess which one a bare `id`
        /// means. `nil` from `tcr peer ls --json`, which sends no such key,
        /// and from an older `status --json`.
        public var display: String?
        public var name: String?
        public var address: String?
        public var trusted: Bool
        /// Unix milliseconds of the last answer. Freshness, not liveness:
        /// sleeping is the normal state for a laptop and there is no peer-down
        /// event anywhere in this design.
        public var lastSeenMs: Int64?
        /// This Mac may route its own traffic through that one, blind.
        public var carries: Bool
        /// That Mac may serve this Mac's requests on its own accounts, and
        /// read them (`allow.disclose`).
        public var serves: Bool
        /// It is serving one right now.
        public var inFlight: Int?
        /// Fraction of the window offered that has been spent, 0 to 1.
        public var leaseSpent: Double?
        /// Seconds left on the current grant before the borrower asks again.
        public var leaseTtlSeconds: Int?
        /// Bytes carried in the last hour, and the ceiling. Gateway rows only.
        public var bytesPerHour: Int64?
        public var byteCapPerHour: Int64?
        /// The lender says it has nothing spare.
        public var noHeadroom: Bool
        /// Decision row 13's end for the lease this row BORROWS, unix
        /// seconds. `nil` is "no end", and also "this `tcr` does not send the
        /// key yet".
        ///
        /// The one time field a borrower may know: the wire `Lease` carries
        /// `until` and nothing else that names a clock, so the borrowing Mac
        /// can say when the work stops. It could not before, and on the scene
        /// where this Mac has nothing spare and depends entirely on the
        /// lender, the work stopped at an hour the screen never named.
        ///
        /// Decoded permissively like every other field here: absent is the
        /// honest absence and draws no clause at all, never a guessed end.
        public var until: Int64?
        /// Whether that borrowed lease's end has passed, as the PRODUCER says.
        ///
        /// `nil` from a `tcr` that does not send it, which is not the same as
        /// `false`: ``leaseHasEnded(now:)`` falls back to the clock only when
        /// the producer stayed silent, so a producer that says "still running"
        /// is believed over this panel's arithmetic about somebody else's
        /// clock.
        public var ended: Bool?
        /// Decision row 12's grants, as the LENDER's own record: one per
        /// lease, each with its scope. Drawn by the per-Mac sheet's "Lend
        /// from" list.
        ///
        /// Empty, not absent, on a Mac with no lease, and empty is also what
        /// a `tcr` without `--scope` answers, which is why the sheet's own
        /// "Add a lease…" row is the only control that appears at zero.
        public var lend: [PeerLendGrant]
        /// Tokens this Mac's leases have actually drawn per hour, as the
        /// LENDER's ledger recorded it.
        ///
        /// The one rate a file can answer: a lease measured in tokens knows
        /// both what it granted and the fraction spent. A lease measured as a
        /// utilization fraction carries no token count anywhere, so those rows
        /// arrive `nil` and the row draws no clause rather than a zero.
        public var tokensPerHour: Int64?
        /// Every way this Mac knows to reach that one, newest first.
        ///
        /// Empty from a `tcr` that predates the block, which reads as "this Mac
        /// reported no way to reach that one", the honest absence, and never
        /// one invented path. The figures on each path are the prober's and
        /// arrive `nil` until it lands; see ``PeerPath``.
        public var paths: [PeerPath]

        public init(
            id: String? = nil, display: String? = nil, name: String? = nil, address: String? = nil,
            trusted: Bool = false,
            lastSeenMs: Int64? = nil, carries: Bool = false, serves: Bool = false,
            inFlight: Int? = nil, leaseSpent: Double? = nil, leaseTtlSeconds: Int? = nil,
            bytesPerHour: Int64? = nil, byteCapPerHour: Int64? = nil, noHeadroom: Bool = false,
            until: Int64? = nil, ended: Bool? = nil, lend: [PeerLendGrant] = [],
            tokensPerHour: Int64? = nil, paths: [PeerPath] = []
        ) {
            self.id = id
            self.display = display
            self.name = name
            self.address = address
            self.trusted = trusted
            self.lastSeenMs = lastSeenMs
            self.carries = carries
            self.serves = serves
            self.inFlight = inFlight
            self.leaseSpent = leaseSpent
            self.leaseTtlSeconds = leaseTtlSeconds
            self.bytesPerHour = bytesPerHour
            self.byteCapPerHour = byteCapPerHour
            self.noHeadroom = noHeadroom
            self.until = until
            self.ended = ended
            self.lend = lend
            self.tokensPerHour = tokensPerHour
            self.paths = paths
        }

        /// One pinned Mac's row, in EITHER of the two spellings this app is
        /// sent.
        ///
        /// `tcr status --json`'s peers block (`PeerStatusRow` in
        /// `src/status.rs`) says `id`, `name`, `address`, `trusted`. The
        /// `tcr peer ls --json` document this type is decoded from says
        /// `node`, `label`, `endpoints[]`, `allow{}` and no `trusted` at all,
        /// which was measured against the built binary rather than read off a
        /// doc-comment:
        ///
        /// ```json
        /// {"addedAt":1,"allow":{"carry":true,"inspect":true,"gateway":false,
        ///  "relay":false,"allowDisclose":false,"acceptMove":false,
        ///  "control":{"briefs":false,"diag":false,"lendable":false}},
        ///  "ended":false,
        ///  "endpoints":[{"addr":"192.0.2.7:41234","kind":"direct",
        ///                "observedAtMs":2,"source":"paired"}],
        ///  "label":"studio-mac","lend":[],"node":"0000…","until":null}
        /// ```
        ///
        /// So every row in a Peers tab decoded to a nil id, a nil name and
        /// `trusted: false`, and the tab drew placeholder rows for Macs the
        /// operator had pinned.
        ///
        /// Both spellings rather than a rename, because both producers are
        /// real and neither is going away: the first key wins and the second
        /// is the fallback. `trusted` has no fallback key and defaults to TRUE
        /// when a `node` was read, because every row in the `peer ls` document
        /// IS a pinned Mac, that file being the record of what this Mac
        /// trusts; a row from the status block still gets its own answer.
        public init(from decoder: Decoder) throws {
            let c = try decoder.container(keyedBy: Keys.self)
            let node = try c.decodeIfPresent(String.self, forKey: .node)
            self.id = try c.decodeIfPresent(String.self, forKey: .id) ?? node
            self.display = try c.decodeIfPresent(String.self, forKey: .display)
            self.name =
                try c.decodeIfPresent(String.self, forKey: .name)
                ?? c.decodeIfPresent(String.self, forKey: .label)
            let allow = try c.decodeIfPresent(Allow.self, forKey: .allow)
            let endpoints = try c.decodeIfPresent([Endpoint].self, forKey: .endpoints) ?? []
            self.address =
                try c.decodeIfPresent(String.self, forKey: .address)
                ?? endpoints.first(where: { $0.addr != nil })?.addr
            self.trusted = try c.decodeIfPresent(Bool.self, forKey: .trusted) ?? (node != nil)
            self.lastSeenMs = try c.decodeIfPresent(Int64.self, forKey: .lastSeenMs)
            self.carries = try c.decodeIfPresent(Bool.self, forKey: .carries) ?? (allow?.carry ?? false)
            // `allow.allowDisclose`, never `allow.inspect`: this field is
            // documented above as "may serve … and read them
            // (allow.disclose)", and `status.rs:790` answers its own
            // `serves` with `row.allow.allow_disclose` too. `inspect` is the
            // OTHER direction of the same grant pair (`Allow::inspect` is
            // "may I read THEIRS"), so a Mac that only allowed peers to
            // inspect its own traffic, and never allowed a peer to serve on
            // its accounts, drew `serves: true` on every `peer ls` fallback.
            self.serves =
                try c.decodeIfPresent(Bool.self, forKey: .serves) ?? (allow?.allowDisclose ?? false)
            self.inFlight = try c.decodeIfPresent(Int.self, forKey: .inFlight)
            self.leaseSpent = try c.decodeIfPresent(Double.self, forKey: .leaseSpent)
            self.leaseTtlSeconds = try c.decodeIfPresent(Int.self, forKey: .leaseTtlSeconds)
            self.bytesPerHour = try c.decodeIfPresent(Int64.self, forKey: .bytesPerHour)
            self.byteCapPerHour = try c.decodeIfPresent(Int64.self, forKey: .byteCapPerHour)
            self.noHeadroom = try c.decodeIfPresent(Bool.self, forKey: .noHeadroom) ?? false
            // Tolerant, and OPTIONAL rather than defaulted: absent is "this
            // tcr does not send it", which the row draws as no clause at all.
            // A `false` default would let a build that knows nothing about
            // ends assert that no lease has one.
            self.until = try c.decodeIfPresent(Int64.self, forKey: .until)
            self.ended = try c.decodeIfPresent(Bool.self, forKey: .ended)
            self.lend = try c.decodeIfPresent([PeerLendGrant].self, forKey: .lend) ?? []
            // OPTIONAL, never `0`: a token rate of zero is a lease that drew
            // nothing, and absent is a `tcr` that cannot answer the question.
            self.tokensPerHour = try c.decodeIfPresent(Int64.self, forKey: .tokensPerHour)
            // Defaulted to EMPTY rather than optional, because the two readings
            // are the same sentence here: a server that sends no paths and a
            // server too old to know the key have both told this Mac no way to
            // reach that one, and the row draws the address line it always did.
            self.paths = try c.decodeIfPresent([PeerPath].self, forKey: .paths) ?? []
        }

        private enum Keys: String, CodingKey {
            case id, display, name, address, trusted, lastSeenMs, carries, serves, inFlight
            case leaseSpent, leaseTtlSeconds, bytesPerHour, byteCapPerHour, noHeadroom
            case until, ended, lend, tokensPerHour, paths
            // The `tcr peer ls --json` spellings of the first four.
            case node, label, endpoints, allow
        }

        /// The `allow` block, for the two flags a row draws.
        ///
        /// Only the two: this type exists to answer "does this Mac carry for
        /// us" and "may it serve on our accounts", and decoding the rest would
        /// be a second copy of a grant table nothing here reads.
        ///
        /// `allowDisclose` is decoded, not `inspect`: `serves` answers "may
        /// that Mac serve on OUR accounts", which is the same direction
        /// `crate::peer::config::Allow::allow_disclose` names ("they may read
        /// MINE"); `inspect` is the other direction of the pair ("I may read
        /// THEIRS") and answers a different question this row does not ask.
        private struct Allow: Decodable {
            var carry: Bool = false
            var allowDisclose: Bool = false
        }

        /// One endpoint, for its address.
        ///
        /// `addr` is absent on a `via` or a `reverse` endpoint, which name
        /// another Mac rather than a socket, so the first endpoint WITH one is
        /// the address to draw.
        private struct Endpoint: Decodable {
            var addr: String?
        }
    }

    /// One way this Mac knows to reach one peer, and what is known about it.
    ///
    /// The identifier and the locator are different facts (the Rust side's
    /// ``PeerEntry/id`` is the pinned key, which never moves), and this is the
    /// locator half: a disposable address the serving process learned, rated,
    /// and may replace without the row becoming a different Mac.
    ///
    /// **Every figure here is a MEASUREMENT and every one of them is optional.**
    /// Nothing in this build measures a path yet, the round trip, the loss
    /// fraction and the two rates get their first writer with the prober, so
    /// they arrive `nil`, and ``PeerFormat/pathLine(_:)`` says `not measured`
    /// rather than drawing a zero. A `0 ms` round trip beside `0 % lost` reads
    /// as a perfect path, which is precisely the sentence an unwritten field
    /// used to print.
    public struct PeerPath: Decodable, Equatable, Sendable {
        /// Which kind of route this is.
        ///
        /// `.unknown(String)` carries the raw token, the rule every enum on
        /// this wire follows (``QuotaState``): a kind added on the Rust side,
        /// a relay is next, must draw as itself, never fail the decode and
        /// blank the tab, and never silently render as `direct`, which would
        /// be a claim about where the bytes went.
        public enum Kind: Equatable, Sendable {
            /// A socket address this Mac dials itself.
            case direct
            /// Reached through another pinned Mac that forwards for us; the
            /// endpoint is that Mac's peer id, not an address.
            case via
            case unknown(String)

            public init(token: String) {
                switch token {
                case "direct": self = .direct
                case "via": self = .via
                default: self = .unknown(token)
                }
            }

            /// The raw wire token, round-tripped so an unknown kind is still
            /// displayable.
            public var token: String {
                switch self {
                case .direct: return "direct"
                case .via: return "via"
                case .unknown(let raw): return raw
                }
            }
        }

        /// The socket address for a ``Kind/direct`` path, or the forwarding
        /// Mac's peer id for a ``Kind/via`` one.
        public var endpoint: String
        public var kind: Kind
        /// Round trip in milliseconds, or `nil` when nothing measured it.
        public var rttMs: Double?
        /// Fraction of probes lost, 0 to 1. A measured `0` is a finding and
        /// reads as `no loss`; `nil` reads as nothing at all.
        public var lossPct: Double?
        /// Bytes carried over THIS path in the last hour. `nil` while the byte
        /// budget charges a peer rather than a path.
        public var bytesPerHour: Int64?
        /// Tokens drawn over THIS path in the last hour. `nil` for the same
        /// reason; the per-peer figure is ``PeerEntry/tokensPerHour``.
        public var tokensPerHour: Int64?
        /// Last time this Mac is known to have reached the peer this way.
        public var lastOkMs: Int64?

        public init(
            endpoint: String, kind: Kind, rttMs: Double? = nil, lossPct: Double? = nil,
            bytesPerHour: Int64? = nil, tokensPerHour: Int64? = nil, lastOkMs: Int64? = nil
        ) {
            self.endpoint = endpoint
            self.kind = kind
            self.rttMs = rttMs
            self.lossPct = lossPct
            self.bytesPerHour = bytesPerHour
            self.tokensPerHour = tokensPerHour
            self.lastOkMs = lastOkMs
        }

        public init(from decoder: Decoder) throws {
            let c = try decoder.container(keyedBy: Keys.self)
            // `endpoint` and `kind` are the only two REQUIRED keys on this
            // wire: a path with no endpoint names no route, and one with no
            // kind cannot be drawn without guessing whether the bytes went
            // direct. An absent endpoint defaults rather than throwing, per
            // this document's tolerance rule, and draws as the honest word.
            self.endpoint = try c.decodeIfPresent(String.self, forKey: .endpoint) ?? "unknown"
            self.kind = Kind(
                token: try c.decodeIfPresent(String.self, forKey: .kind) ?? "direct")
            self.rttMs = try c.decodeIfPresent(Double.self, forKey: .rttMs)
            self.lossPct = try c.decodeIfPresent(Double.self, forKey: .lossPct)
            self.bytesPerHour = try c.decodeIfPresent(Int64.self, forKey: .bytesPerHour)
            self.tokensPerHour = try c.decodeIfPresent(Int64.self, forKey: .tokensPerHour)
            self.lastOkMs = try c.decodeIfPresent(Int64.self, forKey: .lastOkMs)
        }

        private enum Keys: String, CodingKey {
            case endpoint, kind, rttMs, lossPct, bytesPerHour, tokensPerHour, lastOkMs
        }
    }

    public struct AnsweringOn: Decodable, Equatable, Sendable {
        public var peer: String
        public var inFlight: Int

        public init(peer: String, inFlight: Int) {
            self.peer = peer
            self.inFlight = inFlight
        }
    }
}

// MARK: - The live half

extension PeerListDocument {
    /// What one live-peers read produced.
    ///
    /// A SECOND read, folded onto the document, for the reason ``Fleet``'s
    /// sessions half already gives: `tcr peer ls --json` is a projection of two
    /// FILES, and a file cannot answer "how fast is this path right now" or
    /// "how many requests is that Mac serving for me". The serving process can,
    /// and it reports them on its status payload (`src/status.rs`'s
    /// `PeerStatusRow`). Thirteen of the sixteen fields this document decodes
    /// had no writer at all before that block existed, so the tab drew an empty
    /// row while both suites stayed green.
    ///
    /// Folding rather than replacing, and it may only ever ADD: a peer with no
    /// server running must keep the rows, the names and the grants the file
    /// answered for, and a live read that fails must cost the tab nothing.
    public struct LivePeersRead: Equatable, Sendable {
        /// `false` when this `tcr` has no live-peers verb at all, which is a
        /// forward-compat state and never a failure: the tab keeps its file
        /// half and draws no path lines.
        public var supported: Bool
        public var peers: [PeerEntry]
        /// Rows that would not decode, counted rather than silently dropped,
        /// the rule ``Fleet`` applies per account row.
        public var unreadable: Int

        public init(supported: Bool, peers: [PeerEntry] = [], unreadable: Int = 0) {
            self.supported = supported
            self.peers = peers
            self.unreadable = unreadable
        }

        /// The read a missing verb, a spawn failure or an unreadable answer
        /// produces. Named rather than spelled at three call sites.
        public static let unsupported = LivePeersRead(supported: false)
    }

    /// Decode `{"supported": Bool, "peers": [...]}`.
    ///
    /// Rows decode INDIVIDUALLY: one unexpected shape in one Mac's row must
    /// not destroy the other rows, which is the same disproportion
    /// ``Fleet/decode(_:)`` exists to avoid for accounts.
    public static func decodeLivePeers(_ data: Data) throws -> LivePeersRead {
        let top = try JSONSerialization.jsonObject(with: data)
        guard let object = top as? [String: Any] else {
            throw DecodingError.dataCorrupted(
                .init(
                    codingPath: [],
                    debugDescription:
                        "the live peers read emits a JSON object; got \(type(of: top))"))
        }
        // Absent `supported` reads TRUE, the same default the document itself
        // takes: a `tcr` new enough to answer rows does not restate it, and
        // defaulting false would throw away a working read.
        let supported = object["supported"] as? Bool ?? true
        let rows = object["peers"] as? [Any] ?? []
        let decoder = JSONDecoder()
        var peers: [PeerEntry] = []
        var unreadable = 0
        for row in rows {
            guard let rowData = try? JSONSerialization.data(withJSONObject: row) else {
                unreadable += 1
                continue
            }
            if let entry = try? decoder.decode(PeerEntry.self, from: rowData) {
                peers.append(entry)
            } else {
                unreadable += 1
            }
        }
        return LivePeersRead(supported: supported, peers: peers, unreadable: unreadable)
    }

    /// This document with the live read folded over it, keyed on the peer id.
    ///
    /// # The three rules, and why each one is that way round
    ///
    /// 1. **A live value wins where it is present, and an absent one changes
    ///    nothing.** The live half reports what the running process measured;
    ///    where it measured nothing it says nothing, and a `nil` overwriting a
    ///    name the file answered for would make the tab worse the moment a
    ///    server started.
    /// 2. **A live row with no match is APPENDED.** Every live row is a PINNED
    ///    Mac, and a pinned Mac missing from the tab is the one Mac an operator
    ///    is certain to look for. Untrusted rows carry no id (decision row 9),
    ///    so they cannot collide with one.
    /// 3. **An unsupported read is returned unchanged**, not emptied.
    public func mergingLive(_ read: LivePeersRead) -> PeerListDocument {
        // Whether the live half answered is recorded on EVERY path through
        // here, including the two that change nothing else: a read that was
        // not supported is exactly the case a row has to word differently,
        // and it used to be dropped on the floor here.
        guard read.supported, !read.peers.isEmpty else {
            var unread = self
            unread.liveAnswered = read.supported
            return unread
        }
        var out = self
        out.liveAnswered = true
        var live: [String: PeerEntry] = [:]
        for row in read.peers where row.id != nil {
            live[row.id ?? ""] = row
        }
        out.peers = peers.map { entry in
            guard let id = entry.id, let measured = live.removeValue(forKey: id) else {
                return entry
            }
            return entry.overlaid(with: measured)
        }
        // Rule 2, in the live read's own order, after the rows the file knows.
        out.peers += read.peers.filter { row in row.id.map { live[$0] != nil } ?? false }
        return out
    }
}

extension PeerListDocument.PeerEntry {
    /// This row with every value the live half measured written over it, and
    /// nothing else touched.
    ///
    /// Field by field rather than wholesale, because the two halves know
    /// different things: the FILE owns the name, the grants and the lend list,
    /// and the running process owns every measurement. A wholesale replace
    /// would blank the first set on every poll.
    func overlaid(with measured: Self) -> Self {
        var out = self
        out.address = measured.address ?? address
        // The live half is the one document that carries the short form at
        // all (`peer ls --json` sends no `display` key), so it always wins
        // when present.
        out.display = measured.display ?? display
        out.lastSeenMs = measured.lastSeenMs ?? lastSeenMs
        // A grant is a file fact, so a live `false` is only believed when the
        // live half has a row at all, which it does, or this method is not
        // called. Both halves read the same peers file for these two.
        out.carries = measured.carries
        out.serves = measured.serves
        out.inFlight = measured.inFlight ?? inFlight
        out.leaseSpent = measured.leaseSpent ?? leaseSpent
        out.leaseTtlSeconds = measured.leaseTtlSeconds ?? leaseTtlSeconds
        out.bytesPerHour = measured.bytesPerHour ?? bytesPerHour
        out.byteCapPerHour = measured.byteCapPerHour ?? byteCapPerHour
        out.tokensPerHour = measured.tokensPerHour ?? tokensPerHour
        out.noHeadroom = measured.noHeadroom || noHeadroom
        out.until = measured.until ?? until
        out.ended = measured.ended ?? ended
        out.lend = measured.lend.isEmpty ? lend : measured.lend
        out.paths = measured.paths.isEmpty ? paths : measured.paths
        return out
    }
}
