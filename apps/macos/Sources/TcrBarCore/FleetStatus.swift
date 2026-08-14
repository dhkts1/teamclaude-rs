import Foundation

// Decoders for `tcr status --json`.
//
// The wire format is a *bare JSON array*, one object per account. Every field
// name here is the camelCase key the CLI emits (src/cli.rs, the `"quotaState":`
// block); the Rust side owns that spelling, so this file is the one place that
// has to track it.
//
// Four deliberate decisions:
//
//  1. Unknown enum payloads decode to an `.unknown(String)` case that carries the
//     raw text. A `quotaState` variant added on the Rust side must degrade to a
//     visible-but-unstyled label, never to a decode failure that blanks the panel.
//  2. `cacheHitRatio`, `probeError` and `lastStreamError` are genuinely `null` in
//     live output and are optional here. `cacheHitRatio` is null — never `0.0` —
//     when there is nothing to divide by, so an optional is the honest type: a
//     structural absence must not render as a measured zero.
//  3. The four quota fractions (`quota`, `fiveHour`, `sevenDay`, `sevenDayOi`)
//     are null too, on any account nothing has been learned about yet. The Rust
//     side says so outright: `gating_quota` (src/cli.rs) returns `None` "when
//     nothing has been learned yet". They were typed non-optional here because
//     the shape was derived from row `[0]` of a live read, where they happened to
//     be populated; the first never-probed account in the fleet then failed the
//     whole decode with `valueNotFound … Path: [2].quota`.
//  4. Rows decode *individually*. One unexpected null in one account used to
//     destroy all thirteen — a wildly disproportionate failure for a panel whose
//     entire job is to keep showing the fleet. A row that will not decode is
//     counted and surfaced (``Fleet/unreadable``), never silently dropped.

/// How close an account is to its own switch threshold.
///
/// Rust spells these `ok` / `near` / `spent` (`quota_state_token`). `near` is a
/// held account that still has headroom; `spent` is fully consumed until reset.
public enum QuotaState: Equatable, Sendable {
    case ok
    case near
    case spent
    case unknown(String)

    public init(token: String) {
        switch token {
        case "ok": self = .ok
        case "near": self = .near
        case "spent": self = .spent
        default: self = .unknown(token)
        }
    }

    /// The raw wire token, round-tripped so an unknown variant is still displayable.
    public var token: String {
        switch self {
        case .ok: return "ok"
        case .near: return "near"
        case .spent: return "spent"
        case .unknown(let raw): return raw
        }
    }

    /// Ordering used to pick the worst account for the menu-bar glyph.
    public var severity: Int {
        switch self {
        case .ok: return 0
        case .unknown: return 1
        case .near: return 2
        case .spent: return 3
        }
    }
}

extension QuotaState: Decodable {
    public init(from decoder: Decoder) throws {
        let raw = try decoder.singleValueContainer().decode(String.self)
        self.init(token: raw)
    }
}

/// Where the numbers came from.
///
/// `offline` means no server answered, so every *counter* (requests, tokens,
/// cache hit ratio) is structurally zero rather than measured. The quota bars are
/// still real. The UI must label this; rendering a structural zero as a
/// measurement is the exact mistake the CLI documents at length.
public enum StatusSource: Equatable, Sendable {
    case live
    case offline
    case unknown(String)

    public init(token: String) {
        switch token {
        case "live": self = .live
        case "offline": self = .offline
        default: self = .unknown(token)
        }
    }

    public var token: String {
        switch self {
        case .live: return "live"
        case .offline: return "offline"
        case .unknown(let raw): return raw
        }
    }

    /// True when serving counters are structurally zero and must not be read as
    /// measurements.
    public var countersAreStructural: Bool { self != .live }
}

extension StatusSource: Decodable {
    public init(from decoder: Decoder) throws {
        let raw = try decoder.singleValueContainer().decode(String.self)
        self.init(token: raw)
    }
}

/// Health of the account's most recent quota probe.
///
/// Rust owns these tokens (`ProbeStatus::as_str`, src/probe.rs): `never`, `ok`,
/// `error`, `timeout`, `rate-limited`. They are enumerated here rather than
/// pattern-matched as strings so a variant this build has not seen lands in
/// ``unknown(_:)`` deliberately, instead of being mistaken for a good probe.
///
/// The distinction that matters to the capacity summary is ``never`` versus
/// everything else. `error`, `timeout` and `rate-limited` all *keep the
/// last-learned quota bar* — src/probe.rs:252-256 is explicit that a probe 429
/// leaves serving quota untouched — so those rows still carry a real
/// measurement. `never` means the account has never been probed at all (or is
/// not an OAuth account), and its `quotaState: "ok"` is a Rust `#[default]`,
/// not an observation.
public enum ProbeState: Equatable, Sendable {
    case never
    case ok
    case error
    case timeout
    case rateLimited
    case unknown(String)

    public init(token: String) {
        switch token {
        case "never": self = .never
        case "ok": self = .ok
        case "error": self = .error
        case "timeout": self = .timeout
        case "rate-limited": self = .rateLimited
        default: self = .unknown(token)
        }
    }

    /// The raw wire token, round-tripped so an unknown variant is still displayable.
    public var token: String {
        switch self {
        case .never: return "never"
        case .ok: return "ok"
        case .error: return "error"
        case .timeout: return "timeout"
        case .rateLimited: return "rate-limited"
        case .unknown(let raw): return raw
        }
    }

    /// True when this account has been probed at least once, so its numbers can
    /// be the result of an observation rather than of a default.
    ///
    /// An unrecognised token answers `false`: an unseen probe state is not
    /// evidence of anything, and understating capacity is the safe direction.
    public var hasBeenProbed: Bool {
        switch self {
        case .never, .unknown: return false
        case .ok, .error, .timeout, .rateLimited: return true
        }
    }

    /// The probe ran and came back without a usable reading.
    ///
    /// A strict subset of ``hasBeenProbed``, which is also true for `.ok`. The
    /// panel needs the distinction because "no quota reading" has two causes
    /// with two different remedies, and only one of them is what the UNMEASURED
    /// pill means. `Tok.unmeasured` is documented as *never probed* — the remedy
    /// is to wait for the first sweep. A probe that ran and errored is a
    /// different fact, and its remedy is to look at why; labelling it
    /// "unmeasured" tells the operator to wait for something that already
    /// happened.
    ///
    /// `.unknown` is deliberately NOT a failure. An unrecognised token is a
    /// state this build cannot name — which is what `Tok.unknown` exists for —
    /// and calling it a failure would assert an observation nobody made.
    public var isFailure: Bool {
        switch self {
        case .error, .timeout, .rateLimited: return true
        case .never, .ok, .unknown: return false
        }
    }
}

extension ProbeState: Decodable {
    public init(from decoder: Decoder) throws {
        let raw = try decoder.singleValueContainer().decode(String.self)
        self.init(token: raw)
    }
}

/// Rendering for the quota fractions.
///
/// Lives in the core rather than in the view because the *honesty* rule it
/// encodes — an absent measurement never renders as a number — is a property of
/// the data, and a view-private formatter cannot be tested.
public enum QuotaFormat {
    /// The label for a not-measured value. Matches what the CLI prints for the
    /// same case, and what `cacheHitRatio` has always rendered here.
    public static let notMeasured = "n/a"

    /// `0.42` → `"42%"`; `nil` → `"n/a"`.
    ///
    /// A nil fraction must never come back as `"0%"`. Zero is a measurement
    /// meaning "nothing spent"; nil means "nothing known", and the two lead an
    /// operator to opposite decisions.
    public static func percent(_ value: Double?) -> String {
        guard let value else { return notMeasured }
        return "\(Int((value * 100).rounded()))%"
    }

    /// What the quota bar should draw for a fraction.
    ///
    /// The view routes through this rather than branching on the optional
    /// itself, so the rule that matters — a nil fraction is *never* the same
    /// drawing as `0.0` — is a property of the model and can be tested. A
    /// zero-width fill and an unmeasured account look identical on screen, and
    /// they mean opposite things. The fraction is *utilization*, so a zero-width
    /// fill is the most headroom there is — nothing spent — while the other
    /// account has simply never been asked.
    public enum BarFill: Equatable, Sendable {
        /// A real reading, clamped to `0...1` for drawing. The numeric label
        /// beside the bar still shows the unclamped figure.
        case measured(Double)
        /// No reading exists. Drawn as an explicit empty track, not as zero.
        case unmeasured
    }

    public static func barFill(_ value: Double?) -> BarFill {
        guard let value else { return .unmeasured }
        return .measured(min(max(value, 0), 1))
    }
}

/// One rate-limit window currently holding an account out of rotation.
public struct HeldWindow: Decodable, Equatable, Sendable {
    public let window: String
    public let minutesUntilReset: Int
    public let resetAtMs: Int64

    public init(window: String, minutesUntilReset: Int, resetAtMs: Int64) {
        self.window = window
        self.minutesUntilReset = minutesUntilReset
        self.resetAtMs = resetAtMs
    }

    /// The reset instant. `resetAtMs` is a real unix-millisecond timestamp.
    public var resetAt: Date {
        Date(timeIntervalSince1970: Double(resetAtMs) / 1000)
    }

    /// `"7d · resets Fri 15:00 · in 4d 12h"`, evaluated against the current
    /// clock. Views re-render often enough that reading `Date()` here is fine;
    /// every test goes through ``label(now:calendar:locale:timeZone:)``.
    public var countdownLabel: String { label(now: Date()) }

    /// Both halves of the answer: an absolute time answers *when*, a relative
    /// duration answers *how long*. Live horizons run to several days, where a
    /// bare `"in 108h 46m"` is correct and useless, and a bare `"Fri 15:00"`
    /// hides whether that is this Friday or the next one.
    ///
    /// `window` is rendered verbatim — `"7d"`, `"5h"`, or whatever the Rust side
    /// starts emitting. Nothing here pattern-matches it.
    public func label(
        now: Date,
        calendar: Calendar = .current,
        locale: Locale = .current,
        timeZone: TimeZone = .current
    ) -> String {
        let relative = Self.duration(minutes: minutesUntilReset)
        guard resetAtMs > 0 else { return "\(window) · in \(relative)" }
        let absolute = Self.absoluteResetLabel(
            resetAt: resetAt,
            now: now,
            calendar: calendar,
            locale: locale,
            timeZone: timeZone
        )
        return "\(window) · resets \(absolute) · in \(relative)"
    }

    /// `45m` under an hour, `2h 46m` under a day, `4d 12h` beyond it.
    ///
    /// The day tier exists because the fleet really does hold accounts out for
    /// 4-plus days: 6526 minutes rendered as `"108h 46m"` is arithmetic, not
    /// information. Precision drops one unit per tier on purpose — nobody plans
    /// around the minutes of a four-day wait.
    public static func duration(minutes: Int) -> String {
        if minutes <= 0 { return "now" }
        let days = minutes / 1440
        if days > 0 {
            let hours = (minutes % 1440) / 60
            return hours == 0 ? "\(days)d" : "\(days)d \(hours)h"
        }
        let hours = minutes / 60
        let rest = minutes % 60
        if hours == 0 { return "\(rest)m" }
        if rest == 0 { return "\(hours)h" }
        return "\(hours)h \(rest)m"
    }

    /// `17:00` today, `tomorrow 23:00` tomorrow, `Fri 15:00` beyond that.
    ///
    /// The clock format comes from the locale, so a 12-hour user sees `3:00 PM`;
    /// nothing here hardcodes `HH:mm`. Past a week a weekday name would wrap
    /// around and mislead — the live fleet's longest horizon is under five days,
    /// so that case is not reachable today, and the relative half of the label
    /// disambiguates it if it ever becomes so.
    public static func absoluteResetLabel(
        resetAt: Date,
        now: Date,
        calendar: Calendar = .current,
        locale: Locale = .current,
        timeZone: TimeZone = .current
    ) -> String {
        var cal = calendar
        cal.locale = locale
        cal.timeZone = timeZone

        let clock = resetAt.formatted(
            Date.FormatStyle(locale: locale, calendar: cal, timeZone: timeZone).hour().minute()
        )
        if cal.isDate(resetAt, inSameDayAs: now) { return clock }
        if let tomorrow = cal.date(byAdding: .day, value: 1, to: now),
            cal.isDate(resetAt, inSameDayAs: tomorrow)
        {
            return "tomorrow \(clock)"
        }
        let weekday = resetAt.formatted(
            Date.FormatStyle(locale: locale, calendar: cal, timeZone: timeZone)
                .weekday(.abbreviated)
        )
        return "\(weekday) \(clock)"
    }
}

/// One account row of `tcr status --json`.
public struct Account: Decodable, Equatable, Identifiable, Sendable {
    public let name: String
    public let priority: Int
    /// Free-form on the Rust side; displayed verbatim, never pattern-matched.
    public let status: String
    public let disabled: Bool

    /// The Rust side's `AccountStatus` (src/manager/mod.rs:182-198), decoded.
    /// `.other` keeps an unknown future variant readable instead of asserting a
    /// meaning this build cannot support.
    public enum AccountHealth: Equatable, Sendable {
        case active, throttled, needsRelogin
        case other(String)
    }

    /// Computed from `status`, exact match, lowercase — never a substitute for
    /// the raw field, which stays displayed verbatim elsewhere. `"error"` maps
    /// to `.needsRelogin` rather than to a case named after Rust's own word: an
    /// `error` account is what the UNMEASURED pill wears too, and the panel must
    /// not use one word to mean two different facts.
    public var health: AccountHealth {
        switch status.lowercased() {
        case "active": return .active
        case "throttled": return .throttled
        case "error": return .needsRelogin
        default: return .other(status)
        }
    }

    /// The gating window — the most-spent of `5h`, `7d`, `7d_oi`. `null` on the
    /// wire, and `nil` here, when nothing has been learned about this account
    /// yet. Never render it as `0`.
    public let quota: Double?
    public let quotaState: QuotaState
    public let fiveHour: Double?
    public let sevenDay: Double?
    public let sevenDayOi: Double?
    public let held: [HeldWindow]

    public let requests: Int
    public let inputTokens: Int
    public let outputTokens: Int
    public let cacheReadTokens: Int
    /// `null` when `inputTokens` is 0 — an absence, not a measured zero.
    public let cacheHitRatio: Double?

    public let probeStatus: ProbeState
    public let probeError: String?
    public let lastStreamError: String?
    public let streamErrorCount: Int

    public let source: StatusSource
    public let serverSha: String?
    public let serverDirty: Bool?

    public var id: String { name }

    /// Worst-first ordering key. A disabled account is not an alarm — it is an
    /// operator decision — so it sorts below a spent one. This does *not* drive
    /// the menu-bar glyph; see ``Fleet/capacityGlyphState``.
    ///
    /// `needsRelogin` is checked explicitly, not left to fall through to
    /// `quotaState.severity`: a broken account's `quotaState` is Rust's
    /// `#[default]` `"ok"` (the same default a never-probed account carries),
    /// so falling through would have scored it `1` — tied with `disabled`,
    /// the one case this property's own doc-comment names as deliberately
    /// NOT an alarm. A dead credential earns the opposite ranking: worse than
    /// `spent`, since a spent account recovers on its own reset and a broken
    /// one does not without operator action.
    public var severity: Int {
        if disabled { return 1 }
        if health == .needsRelogin { return QuotaState.spent.severity + 2 }
        return quotaState.severity + 1
    }

    /// Row order for the panel: what can serve you, first.
    ///
    /// Deliberately NOT ``severity``. That key answers "which single account is the
    /// worst news" for a glyph, and it collides here — `disabled` returns 1 and an
    /// `ok` account returns `quotaState.severity + 1`, which is also 1 — so sorting
    /// rows by it would interleave parked accounts with healthy ones.
    ///
    /// The panel is read to answer "what can serve right now", so usable accounts
    /// come first and the operator's own parked accounts sink to the bottom.
    /// `unmeasured` sits below `near` because an unprobed account is not capacity,
    /// and above `spent` because it is not known to be exhausted either.
    ///
    /// `needsRelogin` sits between `unknown` and `unmeasured`: it is actionable —
    /// there is a real remedy, a click away — so it belongs above the other
    /// non-serving rows and below anything that can still serve. `disabled` is
    /// checked FIRST: a parked account is an operator decision, not a request for
    /// action, so it always sinks to the bottom regardless of its health.
    public var displayOrder: Int {
        if disabled { return 6 }
        if health == .needsRelogin { return 3 }
        if !hasQuotaEvidence { return 4 }
        switch quotaState {
        case .ok: return 0
        case .near: return 1
        case .unknown: return 2
        case .spent: return 5
        }
    }

    /// The window with the soonest reset, when the account is held.
    public var soonestHold: HeldWindow? {
        held.min(by: { $0.minutesUntilReset < $1.minutesUntilReset })
    }

    /// True when this row's quota is something that was *observed*.
    ///
    /// Two conditions, and both are load-bearing:
    ///
    ///  - `quota != nil` — the CLI emits `null` "when nothing has been learned
    ///    yet" (src/cli.rs:258-271), so a nil gating quota is the absence of a
    ///    measurement, full stop.
    ///  - the account has been probed at least once. `ProbeStatus::Never` is
    ///    Rust's `#[default]` (src/probe.rs:243-245), and it arrives paired with
    ///    a `quotaState` of `"ok"` that is the *same* default rather than a
    ///    finding. A failed, timed-out or rate-limited probe still counts here:
    ///    those paths keep the last-learned bar.
    ///
    /// Anything else is an account we know nothing about, and the fleet must say
    /// so rather than assume the best.
    public var hasQuotaEvidence: Bool {
        quota != nil && probeStatus.hasBeenProbed
    }

    /// In rotation *and* known to have headroom.
    ///
    /// The third clause is the one that is easy to omit and expensive to omit:
    /// an unmeasured account reports `quotaState == .ok` by default, so without
    /// it, enabling a never-probed account makes the fleet claim capacity that
    /// nothing has ever verified.
    public var isReady: Bool {
        !disabled && quotaState == .ok && hasQuotaEvidence
    }
}

/// One bucket of the fleet breakdown line.
///
/// `disabled` is its own bucket rather than a quota state: an operator-disabled
/// account has a `quotaState` like any other, but counting it as `ok` would
/// inflate the capacity the fleet actually has. Every account lands in exactly
/// one bucket, so the buckets always sum to `accounts.count`.
public struct FleetTally: Equatable, Sendable {
    public enum Kind: Equatable, Sendable {
        case ok
        case near
        case spent
        case unknown
        /// The refresh token was rejected — `AccountHealth.needsRelogin`. Its
        /// own bucket, checked BEFORE `hasQuotaEvidence` in `init(account:)`:
        /// these accounts also have no quota reading, and folding them into
        /// `.unmeasured` would tell the operator to wait for a sweep that will
        /// never fix a dead credential.
        case needsRelogin
        /// Enabled, but nothing has ever been measured about it. Its own bucket
        /// because folding it into `ok` is precisely the overclaim this exists
        /// to stop, and folding it into `unknown` would conflate "a quota state
        /// this build cannot name" with "no quota state at all".
        case unmeasured
        case disabled

        public var token: String {
            switch self {
            case .ok: return "ok"
            case .near: return "near"
            case .spent: return "spent"
            case .unknown: return "unknown"
            case .needsRelogin: return "need re-login"
            case .unmeasured: return "unmeasured"
            case .disabled: return "disabled"
            }
        }

        /// The bucket an *enabled* account's quota state falls into.
        init(quotaState: QuotaState) {
            switch quotaState {
            case .ok: self = .ok
            case .near: self = .near
            case .spent: self = .spent
            case .unknown: self = .unknown
            }
        }

        /// The bucket an *enabled* account falls into, measurement included.
        /// An unmeasured account's `quotaState` is a default, so it never
        /// reaches the quota-state mapping at all. Health is checked BEFORE
        /// `hasQuotaEvidence` — a broken account has no quota reading either,
        /// and checking evidence first would land every one of them back in
        /// `.unmeasured`.
        init(account: Account) {
            if account.health == .needsRelogin {
                self = .needsRelogin
            } else {
                self = account.hasQuotaEvidence ? Kind(quotaState: account.quotaState) : .unmeasured
            }
        }
    }

    public let kind: Kind
    public let count: Int

    public init(kind: Kind, count: Int) {
        self.kind = kind
        self.count = count
    }

    /// `"7 spent"`.
    public var label: String { "\(count) \(kind.token)" }
}

/// The decoded fleet, plus the facts that are properties of the *fetch* rather
/// than of any one account.
public struct Fleet: Equatable, Sendable {
    /// A row of the array that would not decode, kept so the panel can admit it
    /// exists. Carries its position and the decoder's own words — an operator
    /// who can see "row 2: valueNotFound … quota" can report a real bug; one who
    /// sees twelve accounts where there were thirteen cannot.
    public struct UnreadableRow: Equatable, Sendable {
        /// Zero-based index in the array `tcr status --json` emitted.
        public let index: Int
        public let message: String

        public init(index: Int, message: String) {
            self.index = index
            self.message = message
        }
    }

    public let accounts: [Account]
    /// Rows that failed to decode. Never dropped silently — see ``unreadableNotice``.
    public let unreadable: [UnreadableRow]

    public init(accounts: [Account], unreadable: [UnreadableRow] = []) {
        self.accounts = accounts
        self.unreadable = unreadable
    }

    /// Decode the array **one row at a time**.
    ///
    /// `JSONDecoder().decode([Account].self, …)` is atomic: a single unexpected
    /// null anywhere in the array throws, and the panel loses the entire fleet.
    /// That is a wildly disproportionate blast radius for a read-only status
    /// display, and it is exactly what happened when one never-probed account
    /// arrived with `"quota": null`. Here each element is decoded on its own, so
    /// one bad row costs one row.
    ///
    /// Malformed *input* is still an error: a payload that is not a JSON array
    /// throws, so `StatusPoller` can keep saying "unreadable output" rather than
    /// reporting an empty fleet, which would read as "you have no accounts".
    public static func decode(_ data: Data) throws -> Fleet {
        let top = try JSONSerialization.jsonObject(with: data, options: [.fragmentsAllowed])
        guard let rows = top as? [Any] else {
            throw DecodingError.typeMismatch(
                [Any].self,
                DecodingError.Context(
                    codingPath: [],
                    debugDescription:
                        "tcr status --json emits a bare JSON array; got \(type(of: top))"
                )
            )
        }

        let decoder = JSONDecoder()
        var accounts: [Account] = []
        var unreadable: [UnreadableRow] = []
        for (index, row) in rows.enumerated() {
            // `data(withJSONObject:)` raises an *Objective-C* exception on a
            // fragment, which Swift cannot catch — so ask first rather than
            // letting a row of `42` take the process down.
            guard JSONSerialization.isValidJSONObject(row) else {
                unreadable.append(
                    UnreadableRow(index: index, message: "row is not a JSON object")
                )
                continue
            }
            do {
                let rowData = try JSONSerialization.data(withJSONObject: row, options: [])
                accounts.append(try decoder.decode(Account.self, from: rowData))
            } catch {
                unreadable.append(UnreadableRow(index: index, message: "\(error)"))
            }
        }
        return Fleet(accounts: accounts, unreadable: unreadable)
    }

    /// How many rows `tcr` sent that this build could not read.
    public var unreadableCount: Int { unreadable.count }

    /// `"1 account unreadable"`, or `nil` when every row decoded. Shown in the
    /// panel: a skipped row must never be indistinguishable from an account that
    /// does not exist.
    public var unreadableNotice: String? {
        guard unreadableCount > 0 else { return nil }
        let noun = unreadableCount == 1 ? "account" : "accounts"
        return "\(unreadableCount) \(noun) unreadable"
    }

    /// Every row carries the same `source`; take the first and say so.
    public var source: StatusSource { accounts.first?.source ?? .unknown("none") }

    public var serverSha: String? { accounts.first?.serverSha }
    public var serverDirty: Bool { accounts.first?.serverDirty ?? false }

    /// The account in the worst shape. Reporting only — the menu-bar glyph is
    /// driven by ``capacityGlyphState``, which is a fleet property, not a
    /// worst-member one.
    public var worst: Account? {
        accounts.max(by: { $0.severity < $1.severity })
    }

    public var enabledAccounts: [Account] { accounts.filter { !$0.disabled } }

    /// The accounts in the order the panel lists them: usable first, parked last.
    ///
    /// Ties break on `priority` so rotation order is preserved *within* a group —
    /// the grouping answers "can this serve me", the tiebreak keeps tcr's own
    /// preference visible. `name` is the final tiebreak purely so the list cannot
    /// reshuffle between two polls that carry identical data, which would read as
    /// flicker.
    public var rowsInDisplayOrder: [Account] {
        accounts.sorted {
            ($0.displayOrder, $0.priority, $0.name)
                < ($1.displayOrder, $1.priority, $1.name)
        }
    }

    // MARK: Capacity summary
    //
    // The panel lists every account, which answers "what is each one doing" but
    // not the question an operator actually opens it for: *can I work right
    // now, and if not, when*. These aggregates are that answer, and they live
    // here rather than in the view so they are unit-testable.

    /// Enabled accounts that are in rotation right now *and* have the
    /// measurement to prove it.
    ///
    /// Two ways to be excluded, and both are real:
    ///
    ///  - Disabled. A disabled account is never capacity, whatever its quota
    ///    says — its `quotaState` keeps reporting `ok` while it sits out of
    ///    rotation.
    ///  - Unmeasured. A never-probed account also reports `ok`, because that is
    ///    Rust's default for both fields, and its quota is `null`. It used to be
    ///    excluded only by being disabled, which made the exclusion an accident
    ///    of the operator's settings rather than a property of the evidence —
    ///    and the panel now has an Enable button. See ``Account/isReady``.
    public var readyCount: Int {
        enabledAccounts.filter(\.isReady).count
    }

    /// Enabled accounts nothing is known about. Reported next to `readyCount`
    /// rather than folded into it: "unknown" and "not ready" are different
    /// answers and lead to different next actions.
    ///
    /// EXCLUDES `needsReloginCount`. A broken account also has no quota
    /// reading, but "unmeasured" tells the operator to wait for a sweep that
    /// will never fix a dead credential — this must stay a property of the
    /// evidence a *live* account is missing, not a catch-all for "no reading".
    public var unmeasuredCount: Int {
        enabledAccounts.filter { !$0.hasQuotaEvidence && $0.health != .needsRelogin }.count
    }

    /// Enabled accounts whose refresh token was rejected — dead credentials,
    /// hard-excluded from selection (`src/manager/select.rs:814`, `:931`).
    /// Reported on the same footing as `unmeasuredCount`: both are reasons an
    /// account is not ready, but they lead to different remedies.
    public var needsReloginCount: Int {
        enabledAccounts.filter { $0.health == .needsRelogin }.count
    }

    /// The denominator of the headline: accounts that *could* serve.
    public var enabledCount: Int { enabledAccounts.count }

    /// The soonest reset among the enabled accounts that are not ready — i.e.
    /// when the fleet next gets capacity back. `nil` when nothing is held.
    public var soonestRecovery: HeldWindow? {
        enabledAccounts
            .filter { !$0.isReady }
            .compactMap(\.soonestHold)
            .min(by: { $0.minutesUntilReset < $1.minutesUntilReset })
    }

    /// `"4 of 12 ready"`, or `"No capacity · next in 2h 48m"` when nothing is
    /// ready. Duration formatting is `HeldWindow`'s, never a second formatter.
    ///
    /// Unmeasured accounts get their own clause instead of being absorbed into
    /// either number. `"No capacity"` in front of an account nobody has ever
    /// probed is a claim the fleet cannot support, so that case says
    /// `"No confirmed capacity"` and names the count.
    public var capacitySummary: String {
        if enabledAccounts.isEmpty { return "No enabled accounts" }
        let unmeasured = unmeasuredCount > 0 ? " · \(unmeasuredCount) unmeasured" : ""
        let needsRelogin = needsReloginCount > 0 ? " · \(needsReloginCount) need re-login" : ""
        if readyCount > 0 { return "\(readyCount) of \(enabledCount) ready\(unmeasured)\(needsRelogin)" }
        if unmeasuredCount > 0 {
            guard let next = soonestRecovery else {
                return "No confirmed capacity\(unmeasured)\(needsRelogin)"
            }
            return "No confirmed capacity\(unmeasured)\(needsRelogin) · next in "
                + HeldWindow.duration(minutes: next.minutesUntilReset)
        }
        guard let next = soonestRecovery else { return "No capacity\(needsRelogin)" }
        return "No capacity\(needsRelogin) · next in \(HeldWindow.duration(minutes: next.minutesUntilReset))"
    }

    /// Which tint the capacity line wears. Near-binary: there is capacity, there
    /// is not, or — the case an unmeasured account creates — nobody knows.
    public var capacityState: QuotaState {
        if enabledAccounts.isEmpty { return .unknown("empty") }
        if readyCount > 0 { return .ok }
        return unmeasuredCount > 0 ? .unknown("unmeasured") : .spent
    }

    /// What the menu-bar glyph shows.
    ///
    /// This is *capacity*, not the worst member. A rotating pool is supposed to
    /// have spent accounts in it — that is the mechanism working, not a fault —
    /// so a worst-account-wins glyph sits at its most alarming setting
    /// permanently and stops meaning anything. Three states:
    ///
    ///  - any account ready → `ok`: work can proceed right now. "Ready" is
    ///    ``Account/isReady``, so a never-probed account can never turn the
    ///    glyph green — its `ok` is a default, and a green menu bar is a promise.
    ///  - none ready, at least one merely `near` → `near`: no capacity this
    ///    instant, but something is close rather than fully spent.
    ///  - none ready, none near, something unmeasured → `unknown`: claiming the
    ///    pool is *out* in front of an account nobody has probed would be a
    ///    certainty the fleet has not earned. `near` outranks it, matching
    ///    ``QuotaState/severity``, where a known-close account is worse news
    ///    than an unknown one.
    ///  - none ready, none near, everything measured → `spent`: the pool is out.
    ///
    /// A fleet with no *enabled* accounts is `unknown` rather than `spent`: an
    /// all-disabled fleet is an operator decision, and matches
    /// ``capacityState``'s treatment of the same case.
    public var capacityGlyphState: QuotaState {
        if enabledAccounts.isEmpty { return .unknown("empty") }
        if readyCount > 0 { return .ok }
        if enabledAccounts.contains(where: { $0.hasQuotaEvidence && $0.quotaState == .near }) {
            return .near
        }
        if unmeasuredCount > 0 { return .unknown("unmeasured") }
        return .spent
    }

    /// Per-bucket counts in fixed severity order, with empty buckets omitted so
    /// a healthy fleet reads just `"12 ok"`.
    public var breakdown: [FleetTally] {
        let disabledCount = accounts.count - enabledCount
        var counts: [FleetTally.Kind: Int] = [:]
        for account in enabledAccounts {
            counts[FleetTally.Kind(account: account), default: 0] += 1
        }
        counts[.disabled] = disabledCount
        let order: [FleetTally.Kind] = [
            .ok, .near, .spent, .unknown, .needsRelogin, .unmeasured, .disabled,
        ]
        return order.compactMap { kind in
            guard let count = counts[kind], count > 0 else { return nil }
            return FleetTally(kind: kind, count: count)
        }
    }

    /// `"4 ok · 1 near · 7 spent · 1 disabled"` — the same content as
    /// ``breakdown``, flattened for tests and for accessibility.
    public var breakdownLabel: String {
        breakdown.map(\.label).joined(separator: " · ")
    }
}
