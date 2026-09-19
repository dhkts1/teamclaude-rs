import Foundation

// Decision rows 12 and 13 (a lease's SCOPE, and its END) as values.
//
// # What is not in this tree yet
//
// `src/main.rs:407-428`'s `PeerLendArgs` today carries `--window`,
// `--fraction`, `--ttl` and `--max-inflight` and NONE of `--scope`, `--for`,
// `--until`, `--list`, `--revoke` or `--relend`; `PeerSwitchArgs` (`:349`) has
// no `--scope`; and `PeerLsJson` (`:1129`) writes no `lend` array and no
// `lentTo` map. All six flags and both fields are still missing from this
// tree's CLI. The panel is coded against the shapes those fields will carry,
// so every control below builds argv a `tcr`
// in this tree will refuse with clap's own "unexpected argument", drawn, as
// the pane already draws a refused verb, as the failure it is rather than as a
// press that looked like it worked. Nothing here invents a second spelling to
// fall back on: a fallback would be a silent divergence from the CLI the moment
// REQUEST lands.

/// What a lease draws from. Decision row 12.
///
/// **The scope never leaves the lender.** It is a field in `tcr-peers.json` and
/// in this argv, and the wire `Lease` carries a window, an amount, the times
/// and a lease id, nothing that names an account. So this type is a LENDER's
/// value: it appears on the per-Mac sheet, in the Sharing defaults sheet, and
/// in `tcr peer ls --json`'s own `lend` array, and never in a frame.
public enum LendScope: Equatable, Hashable, Sendable {
    /// Every account this Mac holds, today's shape, and the default.
    case all
    /// One `tcr group`, pooled. Hot-reloaded like every other group.
    case group(String)
    /// One or more accounts by their SANITIZED label, pooled.
    ///
    /// The labels are the ones `tcr status` prints, never an email and never a
    /// UUID: they land in argv, in the peers file and in a screenshot of this
    /// sheet, and this repository is public.
    case accounts([String])
    /// A scope object the wire sent that this build cannot name (neither
    /// `group` nor `accounts`, a future fourth variant).
    ///
    /// The same shape ``PeerLeaseWindow/unknown`` carries and for the same
    /// reason: `PeerLendGrant`'s decoder used to fold this into ``all``
    /// (`(try? …) ?? .all`), which silently WIDENED a grant the operator had
    /// narrowed the instant this build could not parse the narrowing. The
    /// row read "All accounts" for a lease that, on the lender's own disk,
    /// still covered one account. It is never offered in the scope popup and
    /// ``LeaseDraft/refusal(now:calendar:)`` refuses to save one, the same
    /// gate `terms.window == .unknown` already gets.
    case unknown

    /// The `--scope` value: `all`, `group:work`, `account:alice,bob`.
    ///
    /// Singular `account:` even for a set, because that is the spelling
    /// decision row 12 and the mockup's ledger both use
    /// (`--scope account:<label>[,<label>]`).
    ///
    /// `.unknown` has no argv spelling that means anything: it deliberately
    /// does NOT return `"all"`, which would repeat the exact widening this
    /// case exists to stop if a caller ever reached this arm. It returns a
    /// token no CLI `--scope` grammar accepts, so a bypassed refusal fails
    /// loudly (clap refuses the command) rather than quietly lending
    /// everything. `LeaseDraft/refusal(now:calendar:)` is the real gate and
    /// refuses to save one before this is ever called.
    public var argument: String {
        switch self {
        case .all: return "all"
        case .group(let name): return "group:\(name)"
        case .accounts(let labels): return "account:\(labels.joined(separator: ","))"
        case .unknown: return "unknown-scope-refused-before-argv"
        }
    }

    /// What the scope popup shows: `All accounts`, `Group: work`,
    /// `Account: alice`, `2 accounts`.
    public var label: String {
        switch self {
        case .all: return "All accounts"
        case .group(let name): return "Group: \(name)"
        case .accounts(let labels):
            guard let only = labels.first, labels.count == 1 else {
                return "\(labels.count) accounts"
            }
            return "Account: \(only)"
        case .unknown: return "an unknown scope"
        }
    }

    /// Read `all` / `group:work` / `account:alice,bob` back. `nil` on anything
    /// else, a scope this build cannot name is not silently turned into
    /// `all`, which would widen a lease the operator narrowed.
    public static func parse(_ argument: String) -> LendScope? {
        if argument == "all" { return .all }
        if let name = argument.dropPrefixIfPresent("group:") {
            return name.isEmpty ? nil : .group(name)
        }
        if let list = argument.dropPrefixIfPresent("account:") {
            let labels = list.split(separator: ",").map(String.init).filter { !$0.isEmpty }
            return labels.isEmpty ? nil : .accounts(labels)
        }
        return nil
    }
}

extension LendScope: Decodable {
    /// The WIRE shape `tcr_peer_wire::LendScope` actually serializes
    /// (`crates/tcr-peer-wire/src/lib.rs`, `#[serde(rename_all = "camelCase")]`
    /// over a plain, externally tagged Rust enum): the unit variant `All` is
    /// the bare JSON string `"all"`, and the two variants that carry data are
    /// one-key objects, `{"group":"work"}` and `{"accounts":["alice"]}`.
    ///
    /// **Not the same shape as ``argument``.** That is the CLI's `--scope`
    /// spelling (`all`, `group:work`, `account:alice,bob`), one string with
    /// its own grammar, never what crosses the wire. Decoding the wire shape
    /// as a plain `String` (as this type used to) throws a `typeMismatch` the
    /// instant a real lease is scoped to a group or an account set, which
    /// propagates out of the whole `[PeerLendGrant]` array and blanks the
    /// Peers tab over one narrowed lease.
    public init(from decoder: Decoder) throws {
        // `"all"` decodes as a plain string; anything else is one of the two
        // one-key objects.
        if let single = try? decoder.singleValueContainer(),
            let tag = try? single.decode(String.self)
        {
            guard tag == "all" else {
                throw DecodingError.dataCorruptedError(
                    in: single, debugDescription: "unrecognized scope tag \(tag)")
            }
            self = .all
            return
        }
        let c = try decoder.container(keyedBy: WireKeys.self)
        if let group = try c.decodeIfPresent(String.self, forKey: .group) {
            self = .group(group)
            return
        }
        if let accounts = try c.decodeIfPresent([String].self, forKey: .accounts) {
            self = .accounts(accounts)
            return
        }
        throw DecodingError.dataCorrupted(
            .init(
                codingPath: c.codingPath,
                debugDescription: "scope object carries neither `group` nor `accounts`"))
    }

    private enum WireKeys: String, CodingKey { case group, accounts }
}

extension String {
    fileprivate func dropPrefixIfPresent(_ prefix: String) -> String? {
        hasPrefix(prefix) ? String(dropFirst(prefix.count)) : nil
    }
}

/// Which allowance window a lease is against.
///
/// `5h` and `7d` are UNTIERED and `7d_oi` is Fable's own weekly allowance,
/// which is the reason it is a third choice rather than a checkbox: it is the
/// only window upstream reports per model at all, and the shipped default lends
/// all of it.
public enum PeerLeaseWindow: String, Codable, Equatable, Hashable, CaseIterable, Sendable {
    case fiveHour = "5h"
    case week = "7d"
    case fableWeek = "7d_oi"
    /// A window this build does not know.
    ///
    /// The wire has the same arm and for the same reason
    /// (`crates/tcr-peer-wire/src/lib.rs:522`, `#[serde(other)] Unknown`,
    /// "parse survives, handler refuses"), and this side has to honour that
    /// contract: a `Codable` enum that THREW on an unrecognised string would
    /// fail the whole `tcr peer ls --json` decode, and the panel would
    /// collapse to "tcr could not be read" because one lease named a fourth
    /// allowance.
    ///
    /// It is never silently treated as one of the three. It is not offered in
    /// a picker (see ``allCases``), it says so on screen, and it lends
    /// nothing (``LeaseTerms/standard(for:)``).
    case unknown

    /// The three windows a PICKER offers, `unknown` is a decode outcome, not
    /// a choice an operator can make.
    public static var allCases: [PeerLeaseWindow] { [.fiveHour, .week, .fableWeek] }

    /// The segmented control's own words. The raw value is jargon the SHEET is
    /// allowed to show beside it (the mockup's rule 6), never instead of it.
    public var label: String {
        switch self {
        case .fiveHour: return "5-hour"
        case .week: return "7-day"
        case .fableWeek: return "Fable weekly"
        case .unknown: return "an allowance this build does not know"
        }
    }

    /// The same three windows in the width a one-line readout has: `5h`,
    /// `7d`, `Fable`.
    ///
    /// Only for a row that has already been measured too narrow for
    /// ``label``, ``PeerLease/defaultsLine``. `unknown` keeps saying so
    /// rather than shrinking to a token that looks like a real allowance.
    public var shortLabel: String {
        switch self {
        case .fiveHour: return "5h"
        case .week: return "7d"
        case .fableWeek: return "Fable"
        case .unknown: return "unknown"
        }
    }

    public init(from decoder: Decoder) throws {
        let raw = try decoder.singleValueContainer().decode(String.self)
        self = PeerLeaseWindow(rawValue: raw) ?? .unknown
    }
}

/// When a lease stops, as decision row 13 draws it: `No end`, `For 2 h`,
/// `Until 18:00`.
///
/// **This is not the ttl and the two are on the row together for that reason.**
/// The ttl (300 s) is how often the borrower must ask again; the end is
/// absolute, stored as unix seconds, and at it the lender stops renewing. A
/// row that showed one number would make the operator guess which.
public enum LeaseEnd: Equatable, Hashable, Sendable {
    /// The default. No `--for` and no `--until` in the argv at all: an absent
    /// flag is what "no end" means to the CLI, and sending `--for 0` would be
    /// a second spelling of it.
    case none
    /// `--for 2h`, relative, resolved to an absolute instant by the LENDER so
    /// the panel and the binary cannot disagree about which clock ran.
    case after(String)
    /// `--until 18:00`, a clock time today.
    case until(String)

    public var flags: [String] {
        switch self {
        case .none: return []
        case .after(let span): return ["--for", span]
        case .until(let clock): return ["--until", clock]
        }
    }

    /// What the end popup shows.
    public var label: String {
        switch self {
        case .none: return "No end"
        case .after(let span): return "For \(span)"
        case .until(let clock): return "Until \(clock)"
        }
    }

    /// The END an EDITING draft round-trips through, given the grant's own
    /// stored absolute `until`. Shared by ``LeaseDraft/init(editing:peer:calendar:)``
    /// and `PeersSettingsPane.endFor(_:)`, which is the point: those two used
    /// to carry the same reduction written twice, and a fix landing in one
    /// and not the other is exactly how a sheet and a row come to disagree
    /// about the same lease.
    ///
    /// `--until 18:00` can only ever name a time TODAY (decision row 13, and
    /// the fix to `parse_lend_end` REFUSES a clock already behind now rather
    /// than rolling it to tomorrow). So a stored end that is on some OTHER
    /// calendar day collapsed, the moment it round-tripped through here, to
    /// today's clock, silently moving a lease that ended tomorrow at 18:00 to
    /// end within the next few minutes, or, since the refusal landed, to a
    /// Save that fails on a field the operator never touched.
    ///
    /// The fix keeps the date without inventing a second `--until` spelling
    /// (rejected once already, see ``LeaseDraft/init(editing:peer:calendar:)``):
    /// an end on a different day round-trips as `--for <remaining seconds>`
    /// instead, an EXISTING spelling, resolved from the SAVE instant rather
    /// than the instant this draft was opened. The few hundred milliseconds
    /// between opening the sheet and pressing Save is not a figure any lease
    /// here is precise to. Only an end still due today keeps the clock
    /// spelling, which is what the sheet already showed the operator, and
    /// changing that display for no reason would read like the field itself
    /// had moved.
    public static func editing(until: Int64?, now: Date, calendar: Calendar = .current) -> LeaseEnd {
        guard let until else { return .none }
        let untilDate = Date(timeIntervalSince1970: TimeInterval(until))
        guard calendar.isDate(untilDate, inSameDayAs: now) else {
            let remainingSeconds = max(1, Int(untilDate.timeIntervalSince(now).rounded(.up)))
            return .after("\(remainingSeconds)s")
        }
        return .until(PeerLease.clock(unixSeconds: until, calendar: calendar))
    }
}

/// The four numbers a lease carries, in one value.
///
/// One value and not four parameters, for the reason
/// ``PeerSecretInvocation`` gives about its own two halves: a caller that can
/// pass a fraction without its window can pass a 7-day fraction against the
/// 5-hour allowance, and every control on the sheet writes all four together
/// anyway.
public struct LeaseTerms: Equatable, Hashable, Sendable {
    public var window: PeerLeaseWindow
    /// Fraction of the SCOPE's headroom in that window, 0 to 1. The proxy
    /// clamps it to 0.50; the panel shows what it was told.
    public var fraction: Double
    /// Seconds before the borrower asks again.
    public var ttlSeconds: Int
    /// How many borrowed requests may be open at one moment. Not a rate limit.
    public var maxInFlight: Int

    public init(
        window: PeerLeaseWindow, fraction: Double, ttlSeconds: Int = 300, maxInFlight: Int = 2
    ) {
        self.window = window
        self.fraction = fraction
        self.ttlSeconds = ttlSeconds
        self.maxInFlight = maxInFlight
    }

    /// `simple-surface.md`'s own defaults, so no coder picks them: `7d_oi`
    /// full, `7d` and `5h` at 0.20, ttl 300 s, two in flight.
    public static func standard(for window: PeerLeaseWindow) -> LeaseTerms {
        // A window this build cannot name lends NOTHING. Handing it 0.20
        // would be this panel choosing an amount for an allowance it does not
        // understand, which is the one direction a lease must never be wrong
        // in.
        let fraction: Double
        switch window {
        case .fableWeek: fraction = 1.0
        case .fiveHour, .week: fraction = 0.20
        case .unknown: fraction = 0
        }
        return LeaseTerms(
            window: window, fraction: fraction, ttlSeconds: 300, maxInFlight: 2)
    }

    /// `7-day, 20%` / `Fable weekly, all`. What the numbers popup shows on a
    /// Lend-from row, which is the mockup's own phrasing for scene 63.
    public var label: String {
        "\(window.label), \(fraction >= 1 ? "all" : "\(Int((fraction * 100).rounded()))%")"
    }

    /// The flags, in the order the ledger prints them.
    public var flags: [String] {
        [
            "--window", window.rawValue,
            "--fraction", String(format: "%.2f", fraction),
            "--ttl", "\(ttlSeconds)",
            "--max-inflight", "\(maxInFlight)",
        ]
    }
}

/// One lease as the LENDER recorded it, a row of the per-Mac sheet's
/// "Lend from" list, and of `tcr peer ls --json`'s `lend` array.
///
/// Decision row 12: a trusted Mac may hold several at once, one per scope, so
/// "attic-nuc: 20 % of the `work` group's 7-day, and all of account alice's
/// Fable weekly" is one Mac with two of these.
public struct PeerLendGrant: Decodable, Equatable, Identifiable, Sendable {
    /// The wire's own lease id, which is what `--revoke` and `--relend` take.
    ///
    /// **The wire key is `id`, never `leaseId`.**
    /// [`teamclaude_rs::peer::config::LendGrant::id`] is a `u128` rendered as
    /// lower-case hex under the struct's own `camelCase` rename, which leaves
    /// the field NAME `id` untouched (there is no second word to case-convert).
    /// Decoding `leaseId` here silently read every real grant's id as the
    /// empty string, so `--revoke ""` and `--relend ""` ran against every
    /// lease and two different grants shared one (empty) `Identifiable` id.
    public var leaseId: String
    /// What this grant lends from. See ``LendScope`` for the wire shape.
    public var scope: LendScope
    public var window: PeerLeaseWindow
    public var fraction: Double
    public var ttlSeconds: Int
    public var maxInFlight: Int
    /// Decision row 13's absolute end, unix SECONDS. `nil` is "no end".
    public var until: Int64?
    /// Whether the end has passed. The row stays in the list, greyed, with
    /// Re-lend, row 13 keeps it so the operator can see what was lent.
    public var ended: Bool
    /// How this grant's requests leave (decision row 15). Absent on the wire
    /// is ``LendMode/serve``, which is what every grant written before modes
    /// existed already meant, and what `src/peer/config.rs` skips writing.
    public var mode: LendMode
    /// When the key this grant last handed the borrower stops working, unix
    /// SECONDS, or `nil` when the producer said nothing about one.
    ///
    /// The one fact that makes scene 2c drawable: after a switch back to
    /// serve, the borrower's old key is still good until this instant. `nil`
    /// draws no countdown at all rather than a guessed one; nothing writes
    /// this key yet.
    public var handedKeyUntil: Int64?

    public var id: String { leaseId }

    public var terms: LeaseTerms {
        LeaseTerms(
            window: window, fraction: fraction, ttlSeconds: ttlSeconds, maxInFlight: maxInFlight)
    }

    public init(
        leaseId: String, scope: LendScope, window: PeerLeaseWindow, fraction: Double,
        ttlSeconds: Int = 300, maxInFlight: Int = 2, until: Int64? = nil, ended: Bool = false,
        mode: LendMode = .serve, handedKeyUntil: Int64? = nil
    ) {
        self.leaseId = leaseId
        self.scope = scope
        self.window = window
        self.fraction = fraction
        self.ttlSeconds = ttlSeconds
        self.maxInFlight = maxInFlight
        self.until = until
        self.ended = ended
        self.mode = mode
        self.handedKeyUntil = handedKeyUntil
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: Keys.self)
        self.leaseId = try c.decodeIfPresent(String.self, forKey: .leaseId) ?? ""
        // A scope this build cannot name (an object with neither `group` nor
        // `accounts`, e.g. a future fourth variant) must not throw the whole
        // grant away and hide a live lease, but it must also not become
        // `all`: that used to be `(try? …) ?? .all`, which cannot tell "the
        // key was absent" from "the key was present and failed to parse",
        // since both read as `nil` through `try?`. A real narrow scope this
        // build simply could not name silently WIDENED to every account.
        // Absent is `.all` (the documented default, `skip_serializing_if`
        // on the producer's side); present but unparseable is `.unknown`,
        // which the scope popup never offers and `LeaseDraft.refusal`
        // refuses to save.
        do {
            self.scope = try c.decodeIfPresent(LendScope.self, forKey: .scope) ?? .all
        } catch {
            self.scope = .unknown
        }
        self.window =
            try c.decodeIfPresent(PeerLeaseWindow.self, forKey: .window) ?? .week
        self.fraction = try c.decodeIfPresent(Double.self, forKey: .fraction) ?? 0
        // `ttl` OR `ttlS`, because the producer spells it the second way
        // today (`src/peer/config.rs:422`, `ttl_s` under
        // `rename_all = "camelCase"`), and this decoder also accepts `ttl`.
        // Accepting both is not a fallback: it is one field with two
        // spellings, and the alternative is a decoder that quietly shows the
        // 300 s DEFAULT for every lease whose real ttl it did not read.
        self.ttlSeconds =
            try c.decodeIfPresent(Int.self, forKey: .ttl)
            ?? c.decodeIfPresent(Int.self, forKey: .ttlS)
            ?? 300
        self.maxInFlight = try c.decodeIfPresent(Int.self, forKey: .maxInflight) ?? 2
        self.until = try c.decodeIfPresent(Int64.self, forKey: .until)
        self.ended = try c.decodeIfPresent(Bool.self, forKey: .ended) ?? false
        // Absent is `serve`, the same default the producer's own
        // `skip_serializing_if` writes by omitting the key, and an
        // unrecognised token is `serve` too rather than a decode that throws
        // the whole lease away.
        self.mode = LendMode(token: try c.decodeIfPresent(String.self, forKey: .mode))
        self.handedKeyUntil = try c.decodeIfPresent(Int64.self, forKey: .handedKeyUntil)
    }

    private enum Keys: String, CodingKey {
        case leaseId = "id"
        case scope, window, fraction, ttl, ttlS, maxInflight, until, ended
        case mode, handedKeyUntil
    }

    /// The scope popup's label.
    public var scopeLabel: String { scope.label }

    /// The end popup's label: `No end`, or `ends 19:00` from the stored
    /// absolute instant.
    ///
    /// The STORED fact, in clock time. The sentence beside it says what the
    /// operator wants to know instead, see ``endSentence(now:calendar:)``.
    public func endLabel(calendar: Calendar = .current) -> String {
        guard let until else { return LeaseEnd.none.label }
        return "ends \(PeerLease.clock(unixSeconds: until, calendar: calendar))"
    }

    /// `in 1 h, renews every 300 s, 2 at once`, or, once the end has passed,
    /// `ended 17:30, kept here so you can see what was lent`.
    ///
    /// Both halves in one sentence because they are one row in the mockup and
    /// the renewal cadence is the figure an operator misreads as the end.
    public func endSentence(now: Date, calendar: Calendar = .current) -> String {
        let cadence = "renews every \(ttlSeconds) s, \(maxInFlight) at once"
        guard let until else { return cadence }
        let remaining = Double(until) - now.timeIntervalSince1970
        if ended || remaining <= 0 {
            return "ended \(PeerLease.clock(unixSeconds: until, calendar: calendar)), "
                + "kept here so you can see what was lent"
        }
        return "in \(PeerFormat.span(remaining)), \(cadence)"
    }
}

/// One Mac drawing on one account, `lentTo`'s row, decision row 13's first
/// half.
public struct PeerLentToEntry: Decodable, Equatable, Sendable {
    /// The borrowing Mac's display name, as the account card shows it.
    public var peer: String
    /// The lease's scope as the lender recorded it. Present so the card can be
    /// clicked through to the right sheet; the card itself does not print it.
    ///
    /// Wire-decoded like ``PeerLendGrant/scope``, not a plain `String`:
    /// `peer::lease::LentTo::scope` is `tcr_peer_wire::LendScope`, the same
    /// externally tagged enum (`"all"`, `{"group":"work"}`,
    /// `{"accounts":[...]}`), and reading it as a `String` threw the instant a
    /// real lease was scoped to a group or an account set.
    public var scope: LendScope?
    public var window: PeerLeaseWindow
    public var fraction: Double
    /// Decision row 13's end, unix seconds, when the lease has one.
    public var until: Int64?
    /// Whether that end has passed, the producer's own answer
    /// (`peer::lease::LentTo::ended`, always serialized). Never decoded
    /// before, so an ended lease's row survived every producer restart with
    /// no way for ``PeerLease/lentToLine(_:)`` to stop showing it.
    public var ended: Bool

    public init(
        peer: String, scope: LendScope? = nil, window: PeerLeaseWindow, fraction: Double,
        until: Int64? = nil, ended: Bool = false
    ) {
        self.peer = peer
        self.scope = scope
        self.window = window
        self.fraction = fraction
        self.until = until
        self.ended = ended
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: Keys.self)
        self.peer = try c.decodeIfPresent(String.self, forKey: .peer) ?? ""
        // As lenient as `PeerLendGrant.scope`: an unrecognized shape draws the
        // row with no scope to click through on rather than losing the row.
        self.scope = try? c.decodeIfPresent(LendScope.self, forKey: .scope)
        self.window = try c.decodeIfPresent(PeerLeaseWindow.self, forKey: .window) ?? .week
        self.fraction = try c.decodeIfPresent(Double.self, forKey: .fraction) ?? 0
        self.until = try c.decodeIfPresent(Int64.self, forKey: .until)
        self.ended = try c.decodeIfPresent(Bool.self, forKey: .ended) ?? false
    }

    private enum Keys: String, CodingKey { case peer, scope, window, fraction, until, ended }
}

/// A lease being EDITED, before anything is written.
///
/// Decision row 12 gives a lease a scope, four numbers and an end, and the
/// per-Mac sheet used to write one the moment "Add a lease…" was pressed: one
/// click on a control whose ellipsis promises further input granted that Mac a
/// LIVE lease over every account at the 7-day default, and the operator then
/// narrowed a grant that was already serving. A draft is what the ellipsis
/// promised. Nothing leaves this value until Save hands ``arguments`` to the
/// CLI.
///
/// It is one value rather than five `@State` fields on the sheet for the reason
/// ``LeaseTerms`` gives about its own four: a caller that can hold a fraction
/// without its window can write a 7-day fraction against the 5-hour allowance,
/// and every write here sends the whole lease anyway.
public struct LeaseDraft: Equatable, Identifiable, Sendable {
    /// The Mac this lease is for, as argv names it.
    public let peer: String
    /// The lease being edited, or `nil` for one that does not exist yet.
    ///
    /// Not in the argv either way: `tcr peer lend` writes a lease per scope,
    /// so editing one and adding one are the same write. It is here because
    /// the sheet's identity has to differ between "add" and "edit this row",
    /// or SwiftUI reuses the open sheet's state for the next row.
    public let leaseId: String?
    public var scope: LendScope
    public var terms: LeaseTerms
    public var end: LeaseEnd
    /// How this lease sends (decision row 15). Part of the draft rather than a
    /// second write, for the reason every other field here is: one Save sends
    /// the whole lease, and a mode written by its own call would be a second
    /// write that can half-land.
    public var mode: LendMode

    public var id: String { leaseId ?? "new:\(peer)" }

    /// Whether this draft is a lease that does not exist yet.
    public var isNew: Bool { leaseId == nil }

    /// What Save runs: the whole lease, every flag, one write.
    public var arguments: [String] {
        PeerCommand.lend(peer: peer, scope: scope, terms: terms, end: end, mode: mode)
    }

    public init(
        peer: String, leaseId: String? = nil, scope: LendScope, terms: LeaseTerms,
        end: LeaseEnd = .none, mode: LendMode = .serve
    ) {
        self.peer = peer
        self.leaseId = leaseId
        self.scope = scope
        self.terms = terms
        self.end = end
        self.mode = mode
    }

    /// A new lease, at the shipped default and the widest scope: the one value
    /// an operator can narrow rather than have to discover. Nothing is written
    /// until Save.
    public static func new(peer: String) -> LeaseDraft {
        LeaseDraft(peer: peer, scope: .all, terms: .standard(for: .week), end: .none)
    }

    /// An existing lease, as the lender recorded it.
    ///
    /// The end round-trips through ``LeaseEnd/editing(until:now:calendar:)``,
    /// which keeps the DATE of a lease ending some other day rather than
    /// collapsing it to today's clock; see that function's own doc for why.
    /// Shared with `PeersSettingsPane.endFor(_:)` for the reason a second
    /// hand-written reduction here would risk again: the sheet and the row
    /// disagreeing about the same lease.
    public init(
        editing grant: PeerLendGrant, peer: String, now: Date = Date(),
        calendar: Calendar = .current
    ) {
        self.peer = peer
        self.leaseId = grant.leaseId
        self.scope = grant.scope
        self.terms = grant.terms
        self.end = LeaseEnd.editing(until: grant.until, now: now, calendar: calendar)
        self.mode = grant.mode
    }

    /// Why this draft cannot be written yet, in the operator's words, or `nil`
    /// when it can.
    ///
    /// A refusal rather than a clamp: a sheet that quietly rounded a zero
    /// fraction up to the default would write a grant nobody chose, and one
    /// that wrote an end already in the past would end the lease at the moment
    /// it started. Both are states the drawn controls can reach.
    public func refusal(now: Date, calendar: Calendar = .current) -> String? {
        if scope == .unknown {
            return "This build does not know this lease's scope, so it cannot save it without "
                + "either narrowing or widening what it covers."
        }
        if terms.window == .unknown {
            return "This build does not know that allowance, so it cannot say what a share "
                + "of it would be."
        }
        if terms.fraction <= 0 {
            return "A share of zero lends nothing. Raise it, or leave the lease off."
        }
        if case .until(let clock) = end,
            PeerLease.clockHasPassed(clock, now: now, calendar: calendar)
        {
            return "\(clock) has already passed today, so that end is in the past and the "
                + "lease would stop the moment it started."
        }
        return nil
    }
}

/// The lease surfaces' words and argv, in one place.
public enum PeerLease {
    /// The Sharing section's Defaults row, in ONE line: `5h 20% · 7d 20% ·
    /// Fable full · ttl 5 min`.
    ///
    /// The long spelling (`5-hour 20%, 7-day 20%, Fable weekly full, 5 min`)
    /// wrapped to a second line beside its label and its `Customize…` button,
    /// and in a grouped `Form` a wrapped value is a 64 pt row where a single
    /// line is 40. The budget wants the Defaults row single-line, so the
    /// row shows the short spelling and
    /// the sheet behind `Customize…` shows the words.
    ///
    /// Every figure comes from ``LeaseTerms/standard(for:)``, so this cannot
    /// drift from what the sheet writes; the raw window tokens are the
    /// mockup's own (`5h`, `7d`) and `Fable` stands for `7d_oi`, which is what
    /// the operator calls it.
    public static var defaultsLine: String {
        let per = PeerLeaseWindow.allCases.map { window -> String in
            let terms = LeaseTerms.standard(for: window)
            let amount = terms.fraction >= 1 ? "full" : "\(Int(terms.fraction * 100))%"
            return "\(window.shortLabel) \(amount)"
        }
        let minutes = LeaseTerms.standard(for: .week).ttlSeconds / 60
        return (per + ["ttl \(minutes) min"]).joined(separator: " · ")
    }

    /// `17:30` from unix seconds. The stored fact is absolute; a clock time is
    /// what the operator reads, which is decision row 13 in as many words.
    public static func clock(unixSeconds: Int64, calendar: Calendar = .current) -> String {
        let date = Date(timeIntervalSince1970: Double(unixSeconds))
        let parts = calendar.dateComponents([.hour, .minute], from: date)
        return String(format: "%02d:%02d", parts.hour ?? 0, parts.minute ?? 0)
    }

    /// Whether a `--until` clock time is already behind `now` TODAY.
    ///
    /// `--until` is a clock time and the lender resolves it against its own
    /// day, so "18:00" chosen at 19:00 is an end in the past. The panel cannot
    /// fix that spelling, and it can refuse to send it.
    ///
    /// An unparseable string answers `false`: this is a guard against a known
    /// wrong value, not a validator of the CLI's own grammar, and refusing a
    /// spelling the binary may understand would be this panel overruling it.
    public static func clockHasPassed(
        _ clock: String, now: Date, calendar: Calendar = .current
    ) -> Bool {
        let parts = clock.split(separator: ":")
        guard parts.count == 2, let hour = Int(parts[0]), let minute = Int(parts[1]),
            (0...23).contains(hour), (0...59).contains(minute)
        else { return false }
        let today = calendar.dateComponents([.hour, .minute], from: now)
        guard let nowHour = today.hour, let nowMinute = today.minute else { return false }
        return hour * 60 + minute <= nowHour * 60 + nowMinute
    }

    /// `2 running, 1 ended`, the sheet's own sub-section tag. `nil` with no
    /// lease at all, where the only control is Add a lease.
    public static func lendTag(_ grants: [PeerLendGrant], now: Date) -> String? {
        guard !grants.isEmpty else { return nil }
        let ended = grants.filter { $0.isEnded(now: now) }.count
        let running = grants.count - ended
        var parts: [String] = []
        if running > 0 { parts.append("\(running) running") }
        if ended > 0 { parts.append("\(ended) ended") }
        return parts.joined(separator: ", ")
    }

    /// The label `tcr peer ls --json` masks an account label to when the
    /// label is one it refuses to print.
    ///
    /// `src/main.rs:1987`'s `masked_label` runs every label through
    /// `sanitize_label` and answers this on refusal, and an EMAIL is refused
    /// (`peer_ls_masking_tests`, "alice@example.com" -> "[masked]"), as is a
    /// UUID and anything outside the whitelist. This repository is public and
    /// the peers file is hand-editable JSON, so masking on the way out is
    /// right. What it means HERE is that the key is not always an identifier.
    public static let maskedLabel = "[masked]"

    /// The leases drawn against one account, looked up by that account's own
    /// label.
    ///
    /// **A masked key matches nothing, on purpose.** Every account whose label
    /// the CLI refuses to print arrives under the one key `[masked]`, so on a
    /// Mac with two email-labelled accounts inside leases, that key names both
    /// and identifies neither. Handing it to whichever card asked first would
    /// draw one account's lease on another account's card, a wrong answer
    /// about who may spend an allowance, which is worse than no line at all.
    /// So the card draws nothing and the mismatch is reported rather than
    /// papered over.
    ///
    /// This is a limit of the data, not of the lookup: `lentTo` needs to be
    /// keyed on something an account can be matched by.
    public static func leases(
        forAccountLabel label: String, in lentTo: [String: [PeerLentToEntry]]
    ) -> [PeerLentToEntry] {
        guard label != maskedLabel else { return [] }
        guard let entries = lentTo[label] else { return [] }
        return entries
    }

    /// Where one account's requests leave from, looked up by that account's
    /// own label.
    ///
    /// The masked key matches nothing, for the same reason
    /// ``leases(forAccountLabel:in:)`` refuses it: every account whose label
    /// the CLI will not print arrives under one shared `[masked]` key, so
    /// handing it to whichever card asked first would report one account's
    /// exit pin on another account's card. `nil` draws no row, which is an
    /// absence; a wrong pin would be a claim about where traffic leaves.
    public static func exit(
        forAccountLabel label: String, in exits: [String: AccountExit]
    ) -> AccountExit? {
        guard label != maskedLabel else { return nil }
        return exits[label]
    }

    /// Whether the document carries leases this panel cannot attribute to an
    /// account, the `[masked]` key with rows under it.
    ///
    /// A readout rather than a silent drop: the pane can say "some leases
    /// cannot be shown against their account" instead of an operator seeing an
    /// account they know is lent with no line on it.
    public static func hasUnattributableLeases(_ lentTo: [String: [PeerLentToEntry]]) -> Bool {
        !(lentTo[maskedLabel] ?? []).isEmpty
    }

    /// The account card's line: `Lent to attic-nuc 20 % · studio-mac Fable
    /// weekly`, or `nil` when the account is inside no lease, which is the
    /// state the card draws by having no line at all.
    ///
    /// **One line, not a list.** Two Macs fit and a third becomes `and 1
    /// more`, because the card's job is to say THAT the account is lent and
    /// the sheet's job is to say how much (the mockup's ledger for scene 64).
    /// A full fraction is said in its window's words rather than as `100 %`,
    /// which is where `studio-mac Fable weekly` comes from.
    public static func lentToLine(_ allEntries: [PeerLentToEntry]) -> String? {
        // An ended lease is not still lending the account: `PeerLendGrant`'s
        // own sheet keeps an ended row (greyed, for Re-lend), but this line
        // is read as a PRESENT-TENSE fact by the account card, and the
        // producer's own `ended` says so once it is decoded at all.
        let entries = allEntries.filter { !$0.ended }
        guard !entries.isEmpty else { return nil }
        let shown = entries.prefix(2).map { entry -> String in
            let amount =
                entry.fraction >= 1
                ? entry.window.label
                : "\(Int((entry.fraction * 100).rounded())) %"
            return "\(entry.peer) \(amount)"
        }
        var line = "Lent to " + shown.joined(separator: " · ")
        let hidden = entries.count - shown.count
        if hidden > 0 { line += " and \(hidden) more" }
        return line
    }
}

extension PeerLendGrant {
    /// Whether this grant's end has passed. The producer's own `ended` flag
    /// wins; the clock is the fallback for a producer that sends only `until`.
    public func isEnded(now: Date) -> Bool {
        if ended { return true }
        guard let until else { return false }
        return Double(until) <= now.timeIntervalSince1970
    }
}

extension PeerListDocument.PeerEntry {
    // MARK: Decision row 13, from the BORROWER's side
    //
    // The two questions the tab asks about a row that is serving this Mac:
    // when does it stop, and has it stopped. Here rather than in the view
    // because they are arithmetic over two wire fields, and the test target
    // links `TcrBarCore` alone.

    /// Whether the lease this row borrows has ended.
    ///
    /// The producer's own `ended` wins when it sent one; the clock is the
    /// fallback for a producer that sends only `until`. Same precedence as
    /// ``PeerLendGrant/isEnded(now:)``, and for the same reason: the lender
    /// owns the fact, and this panel's arithmetic about somebody else's clock
    /// is second best.
    public func leaseHasEnded(now: Date) -> Bool {
        if let ended { return ended }
        guard let until else { return false }
        return Double(until) <= now.timeIntervalSince1970
    }

    /// `This lease ends in 1h.` and `nil` when there is no end to name, or
    /// when it has already passed.
    ///
    /// A whole sentence rather than a fragment, because it is appended to
    /// whichever sentence the row's meter is already saying and a fragment
    /// would have to agree with four of them.
    public func endsInSentence(now: Date) -> String? {
        guard let until, !leaseHasEnded(now: now) else { return nil }
        let remaining = Double(until) - now.timeIntervalSince1970
        guard remaining > 0 else { return nil }
        return "This lease ends in \(PeerFormat.span(remaining))."
    }
}

extension LeaseEnded {
    /// Whether this row's lease is over, and what the row then says.
    ///
    /// Two directions, because a row can hold a lease in either: the one this
    /// Mac BORROWS (`until`/`ended` on the row itself) and the ones it LENDS
    /// (`lend`, one per scope). A lent lease is over only when every one of
    /// them is, since a Mac with one live lease and one expired one is still
    /// serving.
    ///
    /// `nil` means no lease has ended, which is also what a row with no lease
    /// at all answers.
    ///
    /// In `TcrBarCore` rather than in the view that draws it because the two
    /// facts worth gating are decisions, not layout: which direction ended,
    /// and whether this Mac may re-lend. A test drives them here; as a private
    /// function on the tab it could only be read as source text, and a
    /// formatter re-wrapping one line would have been indistinguishable from
    /// the rule changing.
    public static func forEntry(
        _ entry: PeerListDocument.PeerEntry, title: String, now: Date,
        calendar: Calendar = .current
    ) -> LeaseEnded? {
        let borrowedEnded =
            (entry.until != nil || entry.ended != nil) && entry.leaseHasEnded(now: now)
        let lent = entry.lend
        let lentEnded = !lent.isEmpty && lent.allSatisfy { $0.isEnded(now: now) }
        guard borrowedEnded || lentEnded else { return nil }

        // The lender's record is the one that can be re-lent, and it is also
        // the one that carries a clock this Mac may state.
        let lastEnded = lent.filter { $0.until != nil }.max { ($0.until ?? 0) < ($1.until ?? 0) }
        let clockSeconds = lastEnded?.until ?? (lentEnded ? nil : entry.until)
        let when = clockSeconds.map { PeerLease.clock(unixSeconds: $0, calendar: calendar) }

        guard lentEnded else {
            // A lease this Mac BORROWED. There is no lease id it may use and
            // no act it may perform: the lender re-lends. A Re-lend button
            // here would be a control for somebody else's decision.
            return LeaseEnded(
                when: when,
                sentence: "The lease \(title) was serving you under has ended, so nothing is "
                    + "being served on it. That Mac is the one that can lend it again.",
                relendArguments: nil)
        }
        // `PeerId::parse` refuses a name and the `tcr-…` display form; only
        // the wire id is a peer argv `tcr` accepts. A row this build has not
        // yet learned the wire id for (an untrusted or freshly-trusted row,
        // see ``PeerEntry/id``'s own doc) gets a line saying why, never a
        // Re-lend button built on `title`, which used to send a value the
        // CLI refuses and read to the operator as a press that did nothing.
        guard let peerId = entry.id else {
            return LeaseEnded(
                when: when,
                sentence: "What you lent \(title) has ended, and it is kept here so you can "
                    + "see what was lent. This build has not learned that Mac's wire id yet, "
                    + "so it cannot Re-lend until it is seen again.",
                relendArguments: nil)
        }
        return LeaseEnded(
            when: when,
            sentence: "What you lent \(title) has ended, and it is kept here so you can see "
                + "what was lent. Re-lend puts it back.",
            relendArguments: lent.first.map {
                PeerCommand.lendRelend(peer: peerId, leaseId: $0.leaseId)
            })
    }
}

extension PeerCommand {
    // MARK: Decision rows 12 and 13's lend verbs

    /// `tcr peer lend <peer> --scope … --window … --fraction … --ttl …
    /// --max-inflight … [--for 2h | --until 18:00]`.
    ///
    /// One write per lease, and the whole lease: the scope popup and the
    /// numbers popup on a Lend-from row are two controls over ONE record, so
    /// both send every flag rather than a delta. A partial write would leave
    /// the peers file holding half of one lease and half of the last.
    /// `--mode` is written only for a HANDED lease, and that asymmetry is the
    /// producer's own: `LendMode::is_serve` is `skip_serializing_if` on the
    /// grant, so a serve lease has never carried the word and every lease this
    /// mesh has already written means serve without it. Two things follow.
    /// One, an existing lease's argv is byte for byte what it was, which is
    /// what four older tests pin. Two, the flag reaches `tcr` only when the
    /// operator actually asked for the handed mode, so a CLI that does not yet
    /// take `--mode` keeps serving every ordinary write and refuses exactly
    /// the one press that asked for something new. That refusal is shown
    /// verbatim on the pane rather than swallowed.
    public static func lend(
        peer: String, scope: LendScope, terms: LeaseTerms, end: LeaseEnd = .none,
        mode: LendMode = .serve
    ) -> [String] {
        let modeFlags = mode == .serve ? [] : ["--mode", mode.argument]
        return ["peer", "lend", peer, "--scope", scope.argument] + terms.flags + end.flags
            + modeFlags
    }

    /// `tcr peer lend <peer> --list`, what the Lend-from list renders, one
    /// row per lease with its id.
    public static func lendList(peer: String) -> [String] {
        ["peer", "lend", peer, "--list"]
    }

    /// `tcr peer lend <peer> --revoke <lease-id>`, one lease ends, the others
    /// stand. Per lease, which is why it sits on the row; it does not end the
    /// trust, which is what Forget is for.
    public static func lendRevoke(peer: String, leaseId: String) -> [String] {
        ["peer", "lend", peer, "--revoke", leaseId]
    }

    /// `tcr peer lend <peer> --relend <lease-id>`, the greyed row's control.
    /// It grants again; it does not remove, which is why the sheet draws it as
    /// an ordinary control and not a destructive one.
    public static func lendRelend(peer: String, leaseId: String) -> [String] {
        ["peer", "lend", peer, "--relend", leaseId]
    }

    /// `tcr peer share --scope … --window … --fraction … --ttl …
    /// --max-inflight …`, the DEFAULT lease, the one every trusted Mac gets
    /// until it has one of its own.
    ///
    /// No `on`/`off` here: the switch is ``share(on:)`` and this is the
    /// Customize sheet's write. Decision row 12 gave the default lease a scope
    /// too, which is the first row of that sheet.
    public static func shareDefaults(scope: LendScope, terms: LeaseTerms) -> [String] {
        ["peer", "share", "--scope", scope.argument] + terms.flags
    }
}
