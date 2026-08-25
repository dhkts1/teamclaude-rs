import Foundation

// Decoders for `tcr status --json`.
//
// The wire format is a *bare JSON array*, one object per account. Every field
// name here is the camelCase key the CLI emits (src/cli.rs, the `"quotaState":`
// block); the Rust side owns that spelling, so this file is the one place that
// has to track it.
//
// Six deliberate decisions:
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
//  5. `cacheHitRatio` staying `null` is also how this file expects the divert
//     budget work to say "too little traffic to trust a ratio" — an account
//     that has taken a handful of requests can compute a real-looking 33% or
//     100% from one or two samples, and that is not a measurement, it is
//     noise wearing a percent sign. The Rust side (`src/cli.rs`) owns the
//     threshold that decides when there is enough traffic to say anything;
//     this file's contract is only that whatever it decides is "not enough"
//     arrives as `null`, same as a genuinely traffic-free account already
//     does. No second wire shape and no second Swift type: the existing
//     nil-means-absent idiom this file already carries end to end —
//     ``QuotaFormat/percent(_:)`` refusing to print `"0%"` for a nil input,
//     ``QuotaFormat/barFill(_:)`` drawing an explicit empty track instead of a
//     zero-width fill — already renders "we don't know" distinctly from "we
//     measured zero" for every caller of this type, with no view change
//     required. Introducing a second word for the same fact ("no signal"
//     beside "n/a") would teach an operator two labels to learn instead of
//     one, for a distinction (never-probed vs. probed-but-not-enough-data)
//     that changes nothing about what they should do next: wait for more
//     signal. If a future wire format needs to keep the *raw* ratio around
//     for diagnostics while still telling the client not to trust it (a
//     companion boolean/string rather than reusing `null`), that decode
//     lives here and should degrade the same way every other unrecognised
//     shape in this file does — never a thrown row.
//  6. The `usage` object (``UsageRow``) is optional at the row, and its
//     `costUsd` is optional inside, and the two nulls mean different things:
//     the row's null is "this account was not measured" — an older server, or
//     an offline read — while `costUsd`'s is "this traffic could not be
//     priced". `window` carries a third: "the server cannot name when this
//     quota window started". None of the three is a zero, and none of them
//     renders as one. Every counter INSIDE a bucket is non-optional, because
//     the Rust side writes all of them together or writes no bucket at all.

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

    /// `102` → `"102"`; `nil` → `"n/a"`.
    ///
    /// The `Int` counterpart to ``percent(_:)``, for a serving counter that
    /// is a QUANTITY in its own right — `requests` on the wire, `null` on
    /// `source == "offline"` — same honesty rule, same reason: `nil` means
    /// "not measured right now", and defaulting it to `0` at the call site
    /// (`\(value ?? 0)`) would silently turn that absence into a claimed
    /// zero-traffic reading, the exact mistake this whole formatter family
    /// exists to rule out. It also exists so `FleetView.swift` never
    /// interpolates a raw `Int?` directly — Swift's default `Optional`
    /// interpolation prints `"Optional(102)"`, a warning-flagged bug that
    /// fires on the STATIC type regardless of whether the value is present,
    /// so `guard`-and-format here is not only the honest choice but the only
    /// one the compiler doesn't complain about.
    ///
    /// NOT used for `streamErrorCount`: that field is a MODIFIER on an error
    /// string already being displayed, not a quantity worth naming on its
    /// own, so its call site suppresses the count entirely on `nil` rather
    /// than printing `"n/a×"` — see the comment at that call site.
    public static func count(_ value: Int?) -> String {
        guard let value else { return notMeasured }
        return "\(value)"
    }

    /// `0.42` → `"$0.42"`; `12.4157` → `"$12.4"`; `120.4` → `"$120"`;
    /// `nil` → `"n/a"`.
    ///
    /// Decimals shrink as the number grows, so a cost keeps two useful figures
    /// at every size and never spends panel width on digits nobody reads: two
    /// decimals under `$10`, one under `$100`, none above. A whole fleet's day
    /// and one account's window go through the SAME formatter, so two costs in
    /// the panel can always be compared without checking how each was rounded.
    ///
    /// `nil` is `"n/a"` for the reason every formatter in this enum gives:
    /// `costUsd` is `null` on the wire when a bucket served requests and not
    /// one of them could be priced (`src/usage.rs`'s `to_wire`), and `"$0.00"`
    /// would report "this traffic was free" about traffic nobody could price.
    /// A zero that IS measured still prints `"$0.00"` — that is a real reading.
    ///
    /// The band is chosen from the ROUNDED figure, not the raw one. Picking it
    /// first printed `"$100.0"` for `99.96` and `"$10.00"` for `9.996`: three
    /// useful figures, one more than this rule promises, on exactly the values
    /// that sit under a band's ceiling. One re-pass is enough — rounding can
    /// lift a figure across one band, never two.
    public static func usd(_ value: Double?) -> String {
        guard let value else { return notMeasured }
        let firstPass = usdDecimals(for: abs(value))
        let rounded = Double(String(format: "%.\(firstPass)f", value)) ?? value
        return "$" + String(format: "%.\(usdDecimals(for: abs(rounded)))f", value)
    }

    private static func usdDecimals(for magnitude: Double) -> Int {
        magnitude < 10 ? 2 : (magnitude < 100 ? 1 : 0)
    }

    /// `812` → `"812"`; `48_000` → `"48k"`; `1_240_000` → `"1.2M"`;
    /// `nil` → `"n/a"`.
    ///
    /// Token counts run to eight digits and the card has room for four, so a
    /// raw count truncates the line it sits on. One decimal below ten of a
    /// unit (`1.2k`, `1.2M`), none above it (`48k`, `12M`): the same
    /// two-useful-figures rule ``usd(_:)`` follows, for the same reason.
    ///
    /// `nil` → `"n/a"`, never `"0"`: an absent count is not a count of zero.
    ///
    /// The band is chosen from the ROUNDED figure, the same rule ``usd(_:)``
    /// follows. Choosing it from the raw count printed `"1000k"` for `999_950`
    /// and `"1000M"` for `999_500_000` — five characters on a line this
    /// function exists to keep to four, and a unit one band below the figure it
    /// names. A band is promoted while the count would round to `1000` of it.
    public static func tokens(_ value: Int?) -> String {
        guard let value else { return notMeasured }
        let bands: [(divisor: Double, suffix: String)] = [
            (1, ""), (1_000, "k"), (1_000_000, "M"), (1_000_000_000, "G"),
        ]
        func printed(_ unit: Double) -> String {
            String(format: "%.\(abs(unit) < 10 ? 1 : 0)f", unit)
        }
        var band = 0
        while band + 1 < bands.count,
            abs(Double(printed(Double(value) / bands[band].divisor)) ?? 0) >= 1_000
        {
            band += 1
        }
        guard band > 0 else { return "\(value)" }
        return printed(Double(value) / bands[band].divisor) + bands[band].suffix
    }

    /// `"claude-sonnet-4-5-20250929"` → `"sonnet-4-5"`;
    /// `"claude-opus-5"` → `"opus-5"`; anything else, verbatim.
    ///
    /// Two removals and no third: the `claude-` prefix, which every model in
    /// this fleet shares and which therefore distinguishes nothing, and a
    /// trailing eight-digit release date, which is the same model. What is
    /// left is the part a reader uses to tell one line from another. An id
    /// this rule does not recognise is printed unchanged rather than guessed
    /// at — a shortened name nobody can look up is worse than a long one.
    public static func modelLabel(_ id: String) -> String {
        var label = id
        if label.hasPrefix("claude-") { label.removeFirst("claude-".count) }
        if let dash = label.lastIndex(of: "-") {
            let tail = label[label.index(after: dash)...]
            if tail.count == 8 && tail.allSatisfy(\.isNumber) {
                label = String(label[..<dash])
            }
        }
        return label
    }

    /// `(3, "overloaded_error")` → `"3× overloaded_error"`;
    /// `(nil, "overloaded_error")` → `"overloaded_error"`.
    ///
    /// Deliberately NOT ``count(_:)`` plus a literal `"×"`: a stream-error
    /// count is a MODIFIER on the error string already being displayed, not
    /// a quantity worth naming when it is absent. The error alone is the
    /// actionable fact regardless of how many times it happened, so an
    /// unmeasured count suppresses the multiplier — `"overloaded_error"` —
    /// rather than rendering ``count(_:)``'s `"n/a"` idiom here, which would
    /// read as the broken-English `"n/a× overloaded_error"` and say nothing
    /// `error` alone doesn't already say. Lives here, not inline in the
    /// view, for the same reason every formatter in this enum does: so the
    /// rendered string is a property of the model a test can assert on
    /// directly.
    public static func streamErrorLabel(count: Int?, error: String) -> String {
        guard let count else { return error }
        return "\(count)× \(error)"
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

    /// `"in 2h 14m"`, or nil when there is nothing to count down to.
    ///
    /// It read `"resets in 2h 14m"` until the card became one line per window.
    /// The caption now follows that window's own percentage (`5h 94% in 2h
    /// 14m`), where the only thing a countdown can count down to is the reset.
    /// The seven characters of `"resets "` are also 35 of the roughly 300pt
    /// that line has, and with them the counters beside it truncated to
    /// `102 req ·…` — a measured fact dropped for a word the context supplies.
    ///
    /// `nil` in → `nil` out — never a placeholder — the same house rule
    /// ``percent(_:)`` states above: an absent measurement never renders as
    /// a fabricated value. A reset at or before `now` also yields `nil`: the
    /// Rust side only ever sends future resets, but a wire value can age
    /// between poll and draw, so this formatter enforces the same "future
    /// only" rule the server already applies, rather than trusting the wire.
    /// Routes through ``HeldWindow/duration(minutes:)`` — the codebase's
    /// single answer to "how long" — instead of growing a second duration
    /// formatter. `now` is a required parameter, not a default, so every
    /// test stays deterministic.
    public static func resetCaption(resetAtMs: Int64?, now: Date) -> String? {
        guard let resetAtMs else { return nil }
        let resetAt = Date(timeIntervalSince1970: Double(resetAtMs) / 1000)
        let seconds = resetAt.timeIntervalSince(now)
        guard seconds > 0 else { return nil }
        let minutes = Int((seconds / 60).rounded())
        guard minutes > 0 else { return nil }
        return "in \(HeldWindow.duration(minutes: minutes))"
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

/// One bucket of measured traffic — a day, a quota window, an hour, or one
/// model's share of the day (`docs/cli.md`, "The `usage` object on `--json`").
///
/// The four token dimensions are kept apart rather than folded into one number
/// because they bill at four different rates, so a single sum could not be
/// priced at all. That is the opposite of the row-level `inputTokens`, which is
/// the QUOTA counter and deliberately folds them together — see
/// ``Account/inputTokens``.
///
/// Every counter here is non-optional, unlike the row-level ones: the Rust side
/// writes all eight keys whenever it writes a bucket at all, so a missing key is
/// a shape this build does not know how to read, and `Fleet.decode` marking that
/// row unreadable is the honest outcome. `costUsd` is the ONE genuine null.
public struct UsageTotals: Decodable, Equatable, Sendable {
    public let requests: Int
    /// BASE input only — cache creation and cache reads are the two fields
    /// below, not folded in here.
    public let inputTokens: Int
    /// Tokens spent WRITING the cache, ALL of it — the 5-minute TTL plus the
    /// 1-hour one. The same quantity the row-level counter carries, so the two
    /// can be compared without knowing which TTL a session asked for.
    public let cacheCreationTokens: Int
    /// The part of ``cacheCreationTokens`` written at the 1-hour TTL. A SUBSET,
    /// never an addend: it is broken out because it bills at a different rate,
    /// and summing the two counts every long-TTL write twice.
    public let cacheCreation1hTokens: Int
    /// Tokens served FROM cache.
    public let cacheReadTokens: Int
    public let outputTokens: Int
    /// API list price for this bucket, in dollars — what the traffic WOULD have
    /// cost on the API, never a bill; these accounts are subscriptions.
    ///
    /// `null`, and `nil` here, when the bucket served requests and not one of
    /// them could be priced (`src/usage.rs`'s `to_wire`). A bucket with some
    /// priced and some unpriced requests reports the partial sum, and
    /// ``unpricedRequests`` is how a reader knows it is partial. Never render
    /// a `nil` as `$0.00` — see ``QuotaFormat/usd(_:)``.
    ///
    /// A bucket that served NOTHING is `0.0`, not `null`: nothing served is a
    /// measured zero. Read it through ``measuredCost`` rather than directly,
    /// which applies that rule to an older proxy's `null` as well.
    public let costUsd: Double?
    /// Requests whose model has no published rate in this build, and which are
    /// therefore MISSING from ``costUsd``. Non-zero means the cost beside it is
    /// a floor, not a total.
    public let unpricedRequests: Int
    /// When this bucket started, unix milliseconds. Present only on
    /// `usage.window`, which is the only bucket whose start is a fact the
    /// server knows (Anthropic's own reset header); `nil` on `today`,
    /// `lastHour` and every per-model bucket, whose spans are defined by their
    /// names.
    public let since: Int64?

    public init(
        requests: Int,
        inputTokens: Int,
        cacheCreationTokens: Int,
        cacheCreation1hTokens: Int,
        cacheReadTokens: Int,
        outputTokens: Int,
        costUsd: Double?,
        unpricedRequests: Int,
        since: Int64? = nil
    ) {
        self.requests = requests
        self.inputTokens = inputTokens
        self.cacheCreationTokens = cacheCreationTokens
        self.cacheCreation1hTokens = cacheCreation1hTokens
        self.cacheReadTokens = cacheReadTokens
        self.outputTokens = outputTokens
        self.costUsd = costUsd
        self.unpricedRequests = unpricedRequests
        self.since = since
    }

    /// Two buckets added, for summing rows into a fleet total.
    ///
    /// Cost follows the server's own rule (`src/usage.rs`'s `Totals::add` and
    /// `to_wire`): a priced side plus an unpriced one is the PARTIAL sum, with
    /// ``unpricedRequests`` carrying the caveat, and the result is `nil` only
    /// when neither side had a price at all. Adding a `nil` as `0` would turn
    /// "nobody could price this" into "this was free". ``since`` is dropped: a
    /// sum of two buckets has no single start instant.
    public func adding(_ other: UsageTotals) -> UsageTotals {
        UsageTotals(
            requests: requests + other.requests,
            inputTokens: inputTokens + other.inputTokens,
            cacheCreationTokens: cacheCreationTokens + other.cacheCreationTokens,
            cacheCreation1hTokens: cacheCreation1hTokens + other.cacheCreation1hTokens,
            cacheReadTokens: cacheReadTokens + other.cacheReadTokens,
            outputTokens: outputTokens + other.outputTokens,
            costUsd: UsageTotals.addCost(costUsd, other.costUsd),
            unpricedRequests: unpricedRequests + other.unpricedRequests,
            since: nil
        )
    }

    /// `nil + nil == nil`; anything else is the sum of what was priced.
    public static func addCost(_ lhs: Double?, _ rhs: Double?) -> Double? {
        switch (lhs, rhs) {
        case (nil, nil): return nil
        case (let a?, let b?): return a + b
        case (let a?, nil): return a
        case (nil, let b?): return b
        }
    }

    /// Cache reads over everything that could have been an input token —
    /// base input, cache writes and cache reads. `nil` when that denominator is
    /// zero, the same honest-null rule ``Account/cacheHitRatio`` follows: no
    /// input means no ratio was measured, not a ratio of zero.
    ///
    /// ``cacheCreation1hTokens`` is NOT added: it is a subset of
    /// ``cacheCreationTokens``, not a second dimension beside it, and adding
    /// both counted every 1-hour cache write twice — which understated the hit
    /// rate on exactly the accounts using the long TTL.
    public var cacheHitRatio: Double? {
        let denominator = inputTokens + cacheCreationTokens + cacheReadTokens
        guard denominator > 0 else { return nil }
        return Double(cacheReadTokens) / Double(denominator)
    }

    /// The cost to RENDER for this bucket: ``costUsd`` when the server priced
    /// it, `0` when the bucket served nothing at all, and `nil` only when
    /// requests were served and none of them could be priced.
    ///
    /// A bucket with zero requests is a measured zero — nothing was served, so
    /// nothing was spent — and `"n/a"` is the token this panel reserves for
    /// "this traffic could not be priced". The two states must not look alike,
    /// and they did: the day accumulator holds the previous day until the first
    /// request of the new one lands, so every night between midnight and that
    /// request the header read `n/a today` about a fleet that was simply idle.
    ///
    /// The server now sends `0.0` for that case (`src/usage.rs`, `Totals` with
    /// no requests), so this is also what keeps an OLDER proxy's `null` from
    /// reading as unpriceable traffic — the same forward-compat contract every
    /// other field here follows.
    public var measuredCost: Double? {
        if let costUsd { return costUsd }
        return requests == 0 ? 0 : nil
    }
}

/// One account's measured spend — the `usage` object on its row.
///
/// `nil` on ``Account/usage`` means NOT MEASURED, never "spent nothing", and
/// the two causes are both ordinary: the row came from the offline path, where
/// there is no serving process to aggregate anything, or the proxy that
/// answered was built before this field existed — which is the routine state
/// here, since the binary on disk is rebuilt on merge while the live process
/// keeps serving until someone restarts it. `source` on the same row says
/// which. Synthesized `Decodable` calls `decodeIfPresent` for an `Optional`
/// property, so the older server's missing key yields `nil` rather than
/// throwing the whole row away — the same forward-compat contract `groups`
/// and the per-window states already follow.
public struct UsageRow: Decodable, Equatable, Sendable {
    /// The local calendar day of the machine the SERVER runs on.
    public let today: UsageTotals
    /// This account's current 5-hour quota window, read from Anthropic's own
    /// reset header. `nil` when that reset is unknown, because then the
    /// window's start cannot be named — a different fact from "the window is
    /// empty", and the card falls back to ``today`` rather than drawing a zero.
    public let window: UsageTotals?
    /// The trailing 60 minutes. Burn rate is `lastHour.costUsd` per hour, by
    /// definition — no division, no assumed span.
    public let lastHour: UsageTotals
    /// ``today``, split by model id. Keys are raw ids (`claude-opus-5`);
    /// ``QuotaFormat/modelLabel(_:)`` shortens them for display.
    public let todayByModel: [String: UsageTotals]

    public init(
        today: UsageTotals,
        window: UsageTotals?,
        lastHour: UsageTotals,
        todayByModel: [String: UsageTotals]
    ) {
        self.today = today
        self.window = window
        self.lastHour = lastHour
        self.todayByModel = todayByModel
    }

    /// The bucket the card's 5h line shows: this account's own quota window
    /// when the server could name its start, else the day. Never `nil` — a row
    /// that carries `usage` at all has a `today`.
    ///
    /// Read ``windowOrTodaySpan`` with it. The fallback is up to 24 hours of
    /// spend rendered beside a 5-hour percentage and a 5-hour countdown, and
    /// unmarked it reads as the window's — three times the true burn rate, late
    /// in the day. Every label built from this bucket says which span it is.
    public var windowOrToday: UsageTotals { window ?? today }

    /// Which span ``windowOrToday`` returned. `.day` is the fallback, and no
    /// string built from a `.day` bucket may say "window".
    public var windowOrTodaySpan: UsageSpan { window == nil ? .day : .window }

    /// The span a rendered figure covers — the discriminator that keeps a day
    /// figure from wearing the `5h` row's label.
    public enum UsageSpan: Equatable, Sendable {
        case window, day
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
    /// Per-window quota state, additive alongside `quotaState` (which stays the
    /// combined most-spent-of-both gating verdict). `nil` both when the wire
    /// field is absent — an older `tcr` this newer TcrBar talks to, same
    /// forward-compat contract every other field in this struct follows, see
    /// decision \#3 above — and when the window itself has no reading yet.
    /// Synthesized `Decodable` already calls `decodeIfPresent` for an
    /// `Optional` property, so no field is ever missing from a decode; a
    /// `nil` here falls back to the shared `quotaState` tint for that bar.
    public let fiveHourState: QuotaState?
    public let sevenDayState: QuotaState?
    /// Each window's own reset instant, UNCONDITIONAL — carried whenever the
    /// window has a live reset, regardless of whether it is currently a
    /// binding hold (`held`, below, still gates on threshold and answers a
    /// different question). `nil` both when the wire field is absent — an
    /// older `tcr` this newer TcrBar talks to, same forward-compat contract
    /// every other field in this struct follows — and when there is no live
    /// reset to report (`src/cli.rs`'s `fiveHourResetAtMs`/`sevenDayResetAtMs`).
    public let fiveHourResetAtMs: Int64?
    public let sevenDayResetAtMs: Int64?
    public let held: [HeldWindow]

    /// Pure serving counters. `null` on the wire — and `nil` here, NEVER `0` —
    /// on `source == .offline`: these four live in the SERVING process
    /// (`src/cli.rs:1179-1209`), so a fresh offline `Manager` has nothing to
    /// report about them, and `0` would read as "this account served nothing"
    /// rather than "not measured right now". Was `Int`, non-optional, until a
    /// synthesized `Decodable` threw `valueNotFound` on the very first offline
    /// row this shipped against — every account on the offline path failed to
    /// decode at once, exactly the failure mode decision #4 above exists to
    /// contain for one bad row, not for every row failing identically.
    /// `fetch_live_status` (Rust) falls back to `offline` on `NoAnswer`/
    /// `Unusable` too, not only `NoServer` — a slow or wedged proxy, i.e.
    /// precisely when an operator most needs the panel to say something.
    public let requests: Int?
    public let inputTokens: Int?
    public let outputTokens: Int?
    public let cacheReadTokens: Int?
    /// `null` when `inputTokens` is 0 — an absence, not a measured zero.
    public let cacheHitRatio: Double?

    /// What this account SPENT, as the proxy measured it while serving. `nil`
    /// means not measured — see ``UsageRow`` for the two ways that happens.
    /// Never read as zero: the panel draws nothing at all for a `nil` here.
    public let usage: UsageRow?

    public let probeStatus: ProbeState
    public let probeError: String?
    public let lastStreamError: String?
    /// Same offline-null idiom as ``requests`` and friends above, and the
    /// field the repo's own `src/cli.rs` test
    /// (`offline_status_reports_null_not_zero_stream_errors`) was written to
    /// guard: `0` on an ERROR counter is an affirmative all-clear, so an
    /// unmeasured `0` here would publish "no truncated streams" about a
    /// process this build never spoke to. Already nullable on the wire before
    /// the four counters above joined it — was already the one field this
    /// struct decoded wrong.
    public let streamErrorCount: Int?

    public let source: StatusSource
    public let serverSha: String?
    public let serverDirty: Bool?

    /// Group labels (`src/stats.rs`'s `AccountSnapshot::groups`, carried onto
    /// the wire as `"groups"`, always an array — `[]` for an unlabelled
    /// account, never `null`). Optional here, deliberately, unlike the wire's
    /// own `[]`-never-`null` rule: synthesized `Decodable` would *throw* on
    /// this whole row against an older `tcr` that predates the field and
    /// omits the key outright, the same failure mode decision \#3 above
    /// exists to prevent. `nil` means "this server did not report groups";
    /// `[]` means "reported, and there are none" — both render as ungrouped,
    /// but the type keeps the distinction rather than collapsing it.
    public let groups: [String]?

    /// Which of this account's own `groups` are held out of the general
    /// pool (`src/stats.rs`'s reserved-group flag, carried onto the wire as
    /// `"reservedGroups"`, repeated per row exactly the way `"groups"`
    /// itself already is — not a group-scoped lookup). Optional for the same
    /// forward-compat reason `groups` is: synthesized `Decodable` would
    /// throw against a server built before this field existed, and that
    /// server's rows must keep decoding. `nil` and `[]` both mean "nothing
    /// reserved" — this build has no server old enough to send `groups` but
    /// not this, but the two fields are decoded identically on principle.
    public let reservedGroups: [String]?

    /// Which of this account's own `groups` have opted in to letting an
    /// explicit `--group` ask select the control account
    /// (`groupSettings.<g>.allowControlAccount`, carried on the wire as
    /// `"controlAllowedGroups"`). Repeated per row exactly the way `groups`
    /// and `reservedGroups` already are, and optional for the same
    /// forward-compat reason: a server built before the opt-in existed sends
    /// no such key, and its rows must keep decoding rather than throwing the
    /// panel back to a fabricated offline snapshot.
    public let controlAllowedGroups: [String]?

    /// Every fleet group mapped to its resolved colour (`"#32d74b"`), carried
    /// onto the wire as `"groupColors"`, repeated per row exactly like
    /// `groups` and `reservedGroups` are — not a group-scoped lookup this
    /// struct has any other source for. The SERVER resolves the colour, not
    /// this app: every client then agrees on what a group looks like, so
    /// nothing here is allowed to invent a colour when this dictionary lacks
    /// one — see ``GroupTag/background`` and ``AccountRow``'s chip, which
    /// fall back to a neutral token instead. Optional for the same
    /// forward-compat reason `groups` is: synthesized `Decodable` would throw
    /// against a server built before this field existed. `nil` when the wire
    /// key is absent, `[:]` when it is present and empty — both leave every
    /// group's ``GroupTag/background`` `nil`.
    public let groupColors: [String: String]?

    /// Explicit memberwise init, needed only because adding `fiveHourState`/
    /// `sevenDayState` after the struct already had test fixtures constructing
    /// it directly would otherwise force every one of them to grow two new
    /// arguments. Both default to `nil` — the same "no per-window reading"
    /// value the wire's absent-field case decodes to — so every pre-existing
    /// call site keeps compiling unchanged. `Decodable` synthesis is untouched
    /// by this: it is generated independently of this initializer.
    public init(
        name: String,
        priority: Int,
        status: String,
        disabled: Bool,
        quota: Double?,
        quotaState: QuotaState,
        fiveHour: Double?,
        sevenDay: Double?,
        sevenDayOi: Double?,
        fiveHourState: QuotaState? = nil,
        sevenDayState: QuotaState? = nil,
        fiveHourResetAtMs: Int64? = nil,
        sevenDayResetAtMs: Int64? = nil,
        held: [HeldWindow],
        requests: Int?,
        inputTokens: Int?,
        outputTokens: Int?,
        cacheReadTokens: Int?,
        cacheHitRatio: Double?,
        probeStatus: ProbeState,
        probeError: String?,
        lastStreamError: String?,
        streamErrorCount: Int?,
        source: StatusSource,
        serverSha: String?,
        serverDirty: Bool?,
        groups: [String]? = nil,
        reservedGroups: [String]? = nil,
        controlAllowedGroups: [String]? = nil,
        groupColors: [String: String]? = nil,
        usage: UsageRow? = nil
    ) {
        self.name = name
        self.priority = priority
        self.status = status
        self.disabled = disabled
        self.quota = quota
        self.quotaState = quotaState
        self.fiveHour = fiveHour
        self.sevenDay = sevenDay
        self.sevenDayOi = sevenDayOi
        self.fiveHourState = fiveHourState
        self.sevenDayState = sevenDayState
        self.fiveHourResetAtMs = fiveHourResetAtMs
        self.sevenDayResetAtMs = sevenDayResetAtMs
        self.held = held
        self.requests = requests
        self.inputTokens = inputTokens
        self.outputTokens = outputTokens
        self.cacheReadTokens = cacheReadTokens
        self.cacheHitRatio = cacheHitRatio
        self.probeStatus = probeStatus
        self.probeError = probeError
        self.lastStreamError = lastStreamError
        self.streamErrorCount = streamErrorCount
        self.source = source
        self.serverSha = serverSha
        self.serverDirty = serverDirty
        self.groups = groups
        self.reservedGroups = reservedGroups
        self.controlAllowedGroups = controlAllowedGroups
        self.groupColors = groupColors
        self.usage = usage
    }

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
    ///
    /// The fourth clause exists for a credential that dies AFTER being
    /// probed, not before. `probe_account` (`src/manager/probing.rs:128-139`)
    /// early-returns on an `Error` row instead of clearing anything, and
    /// `refresh.rs:93-101` only ever sets `status` — so a rejected refresh
    /// token leaves the LAST-LEARNED `quota` and `probeStatus: .ok` sitting
    /// there unchanged. Without this clause `hasQuotaEvidence` stays true,
    /// `quotaState` reads whatever it last measured (often `.ok`), and the
    /// row reports READY for an account `src/manager/select.rs:931`
    /// hard-excludes and will serve zero requests — the header could then
    /// read "1 of 1 ready · 1 need re-login" in the same frame, the panel
    /// contradicting itself.
    public var isReady: Bool {
        !disabled && health != .needsRelogin && quotaState == .ok && hasQuotaEvidence
    }

    /// Which window a per-window quota state or bar belongs to.
    public enum QuotaWindow: Equatable, Sendable {
        case fiveHour
        case sevenDay
    }

    /// The per-window state word when present, else the shared composite
    /// `quotaState`. NOT by itself the answer to "what should this window's
    /// bar look like" — `fiveHourState`/`sevenDayState` are `nil` both when
    /// this window genuinely has no reading AND when an older `tcr` never
    /// sent the field at all (`decodeIfPresent` collapses both), so on its
    /// own this function cannot tell "no reading, paint neutral" from "old
    /// server, borrow the composite state" and must not be called before
    /// that distinction is made — see ``quotaBarTintSource(for:)``, which
    /// makes it and is what callers outside this file should actually use.
    /// Kept internal-facing (not deprecated/removed) because
    /// `quotaBarTintSource(for:)` still needs exactly this fallback for the
    /// genuine old-server case: a fraction present, its state word absent.
    public func effectiveQuotaState(for window: QuotaWindow) -> QuotaState {
        switch window {
        case .fiveHour: return fiveHourState ?? quotaState
        case .sevenDay: return sevenDayState ?? quotaState
        }
    }

    /// What a window's bar should be tinted with — the answer
    /// `effectiveQuotaState(for:)` alone cannot give. `.unmeasured` when this
    /// window has NO reading (`fiveHour`/`sevenDay` fraction is `nil` — the
    /// same per-window fraction ``hasQuotaEvidence`` reads for the composite
    /// bar, populated on old and new wire alike), `.state` otherwise.
    ///
    /// This is the fix for a real bug: `FleetView`'s `fiveHourTint`/
    /// `sevenDayTint` used to call `effectiveQuotaState(for:)` directly, so
    /// an account whose 7-day window was genuinely spent and whose 5-hour
    /// window had never reported fell through `fiveHourState ?? quotaState`
    /// to the COMPOSITE (7d-driven) state and painted the empty 5h bar red —
    /// exactly the overclaim two-window tinting exists to prevent, and not
    /// exotic: `src/quota.rs` populates the two windows independently from
    /// separate response headers, so one sitting at `None` while its sibling
    /// accumulates is ordinary. Proven with the `01d-unmeasured-window-proof`
    /// golden scene before the fix landed (the 5h outline rendered red) and
    /// pinned here by ``QuotaWindowStateTests``.
    public func quotaBarTintSource(for window: QuotaWindow) -> QuotaBarTintSource {
        let fraction: Double? = {
            switch window {
            case .fiveHour: return fiveHour
            case .sevenDay: return sevenDay
            }
        }()
        guard fraction != nil else { return .unmeasured }
        return .state(effectiveQuotaState(for: window))
    }

    /// What the card's 5h line shows on its right: `"$4.20 · 48k out"` — this
    /// account's spend and output tokens for its current quota window.
    ///
    /// `nil` when ``usage`` is `nil`, and the view then draws NOTHING there.
    /// That is the whole rule this property exists to hold: an account the
    /// proxy could not measure gets an empty slot, not a row of zeros, because
    /// `$0.00 · 0` beside a live account is a claim nobody made.
    ///
    /// Every figure carries its unit, because this slot sits in an HStack
    /// beside a percentage and a countdown: a bare `"900"` there reads as 900
    /// requests, 900 dollars or a second percentage. Tokens are `"48k out"`,
    /// never `"48k"`.
    ///
    /// Three markers, each for a fact the figure alone would hide:
    /// - `" today"` on the cost when the bucket is the DAY, which is the
    ///   fallback taken when the server cannot name this window's start
    ///   (``UsageRow/windowOrTodaySpan``). Unmarked, up to 24 hours of spend
    ///   reads as five hours' worth beside a 5h bar.
    /// - `"+"` after the cost when some of the bucket's requests could not be
    ///   priced — `"$5.61+"` is a floor, not a total. The header's `N unpriced`
    ///   clause cannot cover this: it is fleet-wide and computed from `today`.
    /// - the cost dropped entirely, never zeroed, when NOTHING in the bucket
    ///   could be priced; the token count is still a measurement and prints on
    ///   its own. A bucket that served nothing prints `$0.00` — see
    ///   ``UsageTotals/measuredCost``.
    public var windowUsageLabel: String? {
        guard let usage else { return nil }
        let bucket = usage.windowOrToday
        let tokens = "\(QuotaFormat.tokens(bucket.outputTokens)) out"
        guard let cost = bucket.measuredCost else { return spanned(tokens, usage) }
        let partial = bucket.unpricedRequests > 0 ? "+" : ""
        return "\(spanned(QuotaFormat.usd(cost) + partial, usage)) · \(tokens)"
    }

    /// `"$9.41 today"` for a day bucket, the figure unchanged for a window one.
    private func spanned(_ figure: String, _ usage: UsageRow) -> String {
        usage.windowOrTodaySpan == .day ? "\(figure) today" : figure
    }

    /// ``windowUsageLabel`` for VoiceOver, where `"·"` is punctuation and
    /// `"48k"` is not self-describing. Spoken as part of the row's combined
    /// label, for the same reason the pills are: a fact only a sighted user
    /// gets is half built.
    ///
    /// Carries the same three markers the drawn label does, in words: the span
    /// is `"today:"` rather than `"this window:"` on the day fallback, and a
    /// partially priced bucket is spoken as `"at least $5.61"`.
    public var windowUsageSpokenLabel: String? {
        guard let usage else { return nil }
        let bucket = usage.windowOrToday
        let span = usage.windowOrTodaySpan == .day ? "today" : "this window"
        let tokens = "\(QuotaFormat.tokens(bucket.outputTokens)) output tokens"
        guard let cost = bucket.measuredCost else { return "\(span): \(tokens), cost not priced" }
        let spoken = QuotaFormat.usd(cost)
        let priced = bucket.unpricedRequests > 0 ? "at least \(spoken)" : spoken
        return "\(span): \(priced), \(tokens)"
    }

    /// The row-level "change this account's groups" menu — derived purely
    /// from ``groups``, so a test can assert its shape without touching
    /// SwiftUI (bridge: `docs/plans/stacked-cards-bridge.md`, "put rendered
    /// values and state rules on the model, not in the view"). One
    /// ``AccountGroupMenuAction/remove(group:)`` per membership,
    /// ``AccountGroupMenuAction/removeAll`` only once there is more than one
    /// membership to collapse — a single membership already has its own
    /// removal action, and a second control that does the exact same thing
    /// is noise, not a convenience — and ``AccountGroupMenuAction/addToGroup``
    /// always last. `[.addToGroup]` alone for an account in no group at all:
    /// the missing affordance this round exists to add, so an ungrouped
    /// account gets exactly the one action that applies to it.
    public var groupMenuActions: [AccountGroupMenuAction] {
        let sortedGroups = (groups ?? []).sorted()
        var actions: [AccountGroupMenuAction] = sortedGroups.map { .remove(group: $0) }
        if sortedGroups.count > 1 {
            actions.append(.removeAll)
        }
        actions.append(.addToGroup)
        return actions
    }

    /// The row's own tag list — the entire group-membership UI now that the
    /// dedicated group views are gone (bridge:
    /// `docs/plans/group-tags-bridge.md`, "there is no group view. A group is
    /// metadata on an account, shown as a small colored tag."). Sorted
    /// alphabetically, same as ``groupMenuActions``, so an account in several
    /// groups renders the same tags in the same order on every poll — a
    /// stable order is the whole point of deriving this here rather than
    /// iterating the wire's own (unordered-in-practice) array in the view.
    /// `[]` for an ungrouped account: no "ungrouped" tag, no reserved space —
    /// the bridge is explicit that silence is correct here.
    public var groupTags: [GroupTag] {
        let reserved = Set(reservedGroups ?? [])
        return (groups ?? []).sorted().map { name in
            GroupTag(
                name: name,
                isReserved: reserved.contains(name),
                background: (groupColors?[name]).flatMap(GroupTagColor.parse)
            )
        }
    }
}

/// One entry in ``Account/groupMenuActions``, the row-level context menu
/// that changes which groups an account belongs to — right-click on the
/// account itself, the affordance the bridge specifically asked for because
/// membership used to be reachable only from a section-header menu.
public enum AccountGroupMenuAction: Equatable, Hashable, Sendable {
    /// `tcr group rm <group> <account>` for exactly this one membership.
    case remove(group: String)
    /// One `tcr group rm <group> <account>` per current membership — there
    /// is no server-side "remove this account from every group" call, so
    /// the view issues the same single-membership command this case's
    /// sibling does, once per group.
    case removeAll
    /// Opens the "add to an existing group" submenu.
    case addToGroup
}

/// One colored tag on an account row (``Account/groupTags``) — the entire
/// group UI now that the deck cards, sections and Groups tab are gone.
public struct GroupTag: Equatable, Sendable, Identifiable {
    public let name: String
    /// Held out of the general pool. Must be legible WITHOUT relying on
    /// ``background`` alone — colour already carries the group's identity,
    /// so the view distinguishes a reserved tag by shape/glyph, not hue.
    public let isReserved: Bool
    /// The wire-resolved colour for this group, or `nil` when the server
    /// never sent one (older build, or this group missing from
    /// ``Account/groupColors``) — the view falls back to a neutral token
    /// rather than guessing a colour client-side.
    public let background: GroupTagColor.RGB?

    public init(name: String, isReserved: Bool, background: GroupTagColor.RGB?) {
        self.name = name
        self.isReserved = isReserved
        self.background = background
    }

    public var id: String { name }
}

/// Parsing and legibility for a server-resolved group colour. Kept pure and
/// `Foundation`-only (no `SwiftUI`/`AppKit` import) so it is testable from
/// this package without linking the view layer — the view converts
/// ``RGB`` to a `Color` at the last possible step.
public enum GroupTagColor {
    public struct RGB: Equatable, Sendable {
        /// 0...1, sRGB.
        public let red: Double
        public let green: Double
        public let blue: Double

        public init(red: Double, green: Double, blue: Double) {
            self.red = red
            self.green = green
            self.blue = blue
        }
    }

    /// `"#32d74b"` → an `RGB` in `0...1`. `nil` for anything that is not
    /// exactly `#` followed by six hex digits — a malformed wire value falls
    /// back to the neutral token at the call site rather than crashing or
    /// rendering black-on-black.
    public static func parse(_ hex: String) -> RGB? {
        var s = Substring(hex)
        guard s.hasPrefix("#") else { return nil }
        s = s.dropFirst()
        guard s.count == 6, let value = UInt32(s, radix: 16) else { return nil }
        return RGB(
            red: Double((value >> 16) & 0xff) / 255,
            green: Double((value >> 8) & 0xff) / 255,
            blue: Double(value & 0xff) / 255
        )
    }

    /// True when black text reads better than white on this background.
    ///
    /// WCAG's own relative-luminance formula (gamma-corrected sRGB channels,
    /// weighted 0.2126/0.7152/0.0722 for red/green/blue) crossed against the
    /// 0.5 midpoint recommended for exactly this text-on-background choice.
    /// An operator picks the hue, and this is what keeps an unlucky pick — a
    /// pale yellow, a near-white — from rendering unreadable text instead of
    /// choosing one fixed foreground and hoping.
    public static func isLight(_ rgb: RGB) -> Bool {
        func channel(_ c: Double) -> Double {
            c <= 0.03928 ? c / 12.92 : pow((c + 0.055) / 1.055, 2.4)
        }
        let luminance =
            0.2126 * channel(rgb.red) + 0.7152 * channel(rgb.green) + 0.0722 * channel(rgb.blue)
        return luminance > 0.5
    }
}

/// The two things a quota bar's fill/outline can honestly show: no reading
/// at all, or a real per-window state. Kept distinct from a bare
/// `QuotaState?` so a caller cannot accidentally collapse "unmeasured" into
/// some default `QuotaState` case — the type itself forces handling both.
public enum QuotaBarTintSource: Equatable, Sendable {
    case unmeasured
    case state(QuotaState)
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

    /// ``rowsInDisplayOrder``, with the identity-bound control account (if any)
    /// pinned to the very front — above the rotation pool, ahead of even an
    /// otherwise-first `ok` row.
    ///
    /// `controlName` is not a field on ``Account``: the control account is read
    /// via `tcr control --show` (`ControlAccountController`), a source
    /// deliberately separate from `tcr status --json` — see that controller's
    /// doc-comment. So this takes the name as a parameter instead of reading it
    /// off the row, and the caller (`FleetView`) is the one place that already
    /// holds both a `Fleet` and a `ControlAccountController`.
    ///
    /// A **stable partition**, not a re-sort: everything that isn't the control
    /// row keeps the exact relative order ``rowsInDisplayOrder`` already gave
    /// it. `controlName == nil` — no control account set, or this build cannot
    /// ask (``ControlAccountController/unavailable``, which always pairs with a
    /// `nil` `current`) — leaves the order byte-identical to
    /// ``rowsInDisplayOrder``, which is the common case.
    public func rowsInDisplayOrder(pinning controlName: String?) -> [Account] {
        let ordered = rowsInDisplayOrder
        guard let controlName else { return ordered }
        let control = ordered.filter { $0.name == controlName }
        guard !control.isEmpty else { return ordered }
        let rest = ordered.filter { $0.name != controlName }
        return control + rest
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
    ///
    /// That "not ready" claim is enforced by ``Account/isReady``'s own
    /// `health != .needsRelogin` clause, not merely implied by this count
    /// existing — a credential that dies AFTER being probed keeps its
    /// last-learned `quota` and `quotaState`, so without that clause on
    /// `isReady` an account counted here could ALSO count in `readyCount`,
    /// the exact contradiction ("1 of 1 ready · 1 need re-login") this whole
    /// change exists to rule out.
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
        // `health != .needsRelogin` is load-bearing here too, not just on
        // `isReady`: a credential that dies AFTER being probed keeps its
        // LAST-LEARNED `quotaState`, which can be `.near` — Rust never resets
        // it, `probe_account` (`src/manager/probing.rs:128-139`) early-returns
        // on an `Error` row and `refresh.rs:93-101` only ever sets `status`.
        // Without the health check, a broken account that used to be close to
        // its threshold would amber the glyph for capacity that has since
        // gone to zero, not "close".
        if enabledAccounts.contains(where: {
            $0.hasQuotaEvidence && $0.quotaState == .near && $0.health != .needsRelogin
        }) {
            return .near
        }
        if unmeasuredCount > 0 { return .unknown("unmeasured") }
        return .spent
    }

    /// Per-bucket counts in fixed severity order, with empty buckets omitted so
    /// a healthy fleet reads just `"12 ok"`.
    ///
    /// `.needsRelogin` and `.unmeasured` are excluded from the order: both are
    /// already named by ``capacitySummary`` (`"1 need re-login"`,
    /// `"1 unmeasured"`), and this tally used to name them a second time —
    /// the header line read `"1 of 2 ready · 1 need re-login · 1 ok · 1 need
    /// re-login"`. Every other bucket appears in `capacitySummary` only as a
    /// number folded into `readyCount`, never spelled out on its own, so it
    /// keeps its place here.
    public var breakdown: [FleetTally] {
        let disabledCount = accounts.count - enabledCount
        var counts: [FleetTally.Kind: Int] = [:]
        for account in enabledAccounts {
            counts[FleetTally.Kind(account: account), default: 0] += 1
        }
        counts[.disabled] = disabledCount
        let order: [FleetTally.Kind] = [
            .ok, .near, .spent, .unknown, .disabled,
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

    // MARK: Usage summary
    //
    // What the fleet SPENT, summed from the rows that carry a measurement.
    // Every aggregate below skips a row whose `usage` is nil rather than
    // counting it as zero, and reports nil rather than 0 when no row could be
    // measured at all — the same rule `cacheHitRatio` and the offline counters
    // follow, and the reason the header draws no line against an older server
    // instead of a row of zeros.

    /// The rows the proxy actually measured. Sums below are over these only.
    public var measuredUsage: [UsageRow] { accounts.compactMap(\.usage) }

    /// True when at least one row carries a `usage` object. False against an
    /// older `tcr`, and against a fully offline read — the case where the panel
    /// must say nothing about spend rather than say zero.
    public var hasUsage: Bool { !measuredUsage.isEmpty }

    /// Today's cost across the fleet, in dollars. `nil` when no measured row
    /// could be priced at all — see ``UsageTotals/costUsd``. A fleet where some
    /// rows are priced and some are not reports the partial sum, and
    /// ``todayUnpricedRequests`` is how a reader knows it is partial.
    ///
    /// Summed through ``UsageTotals/measuredCost``, so a row that served
    /// nothing today adds a measured `0` rather than an absence. An idle fleet
    /// reports `$0.00`, which is what it spent; `n/a` here means the traffic
    /// could not be priced.
    public var todayCost: Double? {
        measuredUsage.reduce(nil) { UsageTotals.addCost($0, $1.today.measuredCost) }
    }

    /// The trailing hour's cost across the fleet — the burn rate, per hour by
    /// definition. Same nil rule as ``todayCost``, and the same measured-zero
    /// rule: an hour that served nothing is `$0.00/hr`.
    public var lastHourCost: Double? {
        measuredUsage.reduce(nil) { UsageTotals.addCost($0, $1.lastHour.measuredCost) }
    }

    /// Today's requests that are MISSING from ``todayCost`` because their model
    /// has no published rate in this build. `nil` — not `0` — when no row was
    /// measured, so "nothing unpriced" and "nothing measured" stay different
    /// answers.
    public var todayUnpricedRequests: Int? {
        guard hasUsage else { return nil }
        return measuredUsage.reduce(0) { $0 + $1.today.unpricedRequests }
    }

    /// Today's traffic per model, merged across accounts. Keys are raw model
    /// ids; ``QuotaFormat/modelLabel(_:)`` shortens them for display.
    public var todayByModel: [String: UsageTotals] {
        var merged: [String: UsageTotals] = [:]
        for row in measuredUsage {
            for (model, totals) in row.todayByModel {
                merged[model] = merged[model].map { $0.adding(totals) } ?? totals
            }
        }
        return merged
    }

    /// Today's cache hit rate across the fleet — reads over base input plus
    /// cache writes plus reads. `nil` when that denominator is zero, exactly as
    /// ``Account/cacheHitRatio`` is `null` on the wire when there was no input:
    /// no traffic means no ratio was measured, not a ratio of zero.
    public var todayCacheHitRatio: Double? {
        let today = measuredUsage.map(\.today)
        guard !today.isEmpty else { return nil }
        let reads = today.reduce(0) { $0 + $1.cacheReadTokens }
        // `cacheCreation1hTokens` is a SUBSET of `cacheCreationTokens`, not a
        // dimension beside it — see ``UsageTotals/cacheCreationTokens``.
        let denominator = today.reduce(0) {
            $0 + $1.inputTokens + $1.cacheCreationTokens + $1.cacheReadTokens
        }
        guard denominator > 0 else { return nil }
        return Double(reads) / Double(denominator)
    }

    /// Today's traffic per model with the ids CANONICALIZED — the same map
    /// ``todayByModel`` returns, keyed on what a reader will see.
    ///
    /// ``QuotaFormat/modelLabel(_:)`` drops the `claude-` prefix and an
    /// eight-digit release tail, so `claude-opus-5` and
    /// `claude-opus-5-20250929` are ONE model wearing two ids — which is the
    /// ordinary state during a rollout, some sessions pinning the dated id and
    /// others the alias. Grouping on the raw id and shortening afterwards drew
    /// the same name twice, split its true share in half, and let the `+N`
    /// clause count a model that does not exist.
    public var todayByModelLabel: [String: UsageTotals] {
        var merged: [String: UsageTotals] = [:]
        for (id, totals) in todayByModel {
            let label = QuotaFormat.modelLabel(id)
            merged[label] = merged[label].map { $0.adding(totals) } ?? totals
        }
        return merged
    }

    /// Each model's share of today, biggest first — `[("opus-5", 0.62), …]`,
    /// with `nil` for a model whose traffic could not be priced.
    ///
    /// Share is by COST among the models that have one, because cost is what
    /// the line beside it reports and a share computed in a different unit than
    /// the total it sits next to invites exactly the arithmetic a reader
    /// shouldn't have to do. When NOTHING today could be priced, share falls
    /// back to output tokens: the mix is still a fact worth showing, and saying
    /// so in tokens is honest where inventing a cost would not be.
    ///
    /// An unpriced model is LISTED, after the priced ones and ranked among
    /// themselves by output tokens, with a `nil` share the header renders as
    /// `"?"`. It used to be filtered out entirely, so a fleet running most of
    /// its traffic on a model this build has no rate for read as
    /// `opus-5 100%` — and, because the dropped model never reached
    /// `share.count`, the `+N` clause that exists to say "there are more
    /// models" did not fire either. A model with traffic is never absent from
    /// this line; what is unknown about it is its cost, and `"?"` says exactly
    /// that. ``todayUnpricedRequests`` still says how much of the total is
    /// missing.
    public var todayModelShare: [(model: String, share: Double?)] {
        let byLabel = todayByModelLabel.filter { $0.value.requests > 0 }
        guard !byLabel.isEmpty else { return [] }
        // Biggest first; an exact tie breaks on the name so two polls carrying
        // identical data cannot reshuffle the line.
        func rank(
            _ entries: [(model: String, weight: Double, share: Double?)]
        ) -> [(model: String, share: Double?)] {
            entries
                .sorted { $0.weight == $1.weight ? $0.model < $1.model : $0.weight > $1.weight }
                .map { (model: $0.model, share: $0.share) }
        }
        let priced = byLabel.filter { $0.value.costUsd != nil }
        let pricedTotal = priced.values.reduce(0.0) { $0 + ($1.costUsd ?? 0) }
        guard pricedTotal > 0 else {
            let total = byLabel.values.reduce(0) { $0 + $1.outputTokens }
            guard total > 0 else { return [] }
            return rank(
                byLabel.map {
                    let share = Double($0.value.outputTokens) / Double(total)
                    return (model: $0.key, weight: share, share: Double?.some(share))
                })
        }
        let head = rank(
            priced.map {
                let share = ($0.value.costUsd ?? 0) / pricedTotal
                return (model: $0.key, weight: share, share: Double?.some(share))
            })
        let tail = rank(
            byLabel.filter { $0.value.costUsd == nil }
                .map { (model: $0.key, weight: Double($0.value.outputTokens), share: nil) })
        return head + tail
    }

    /// `"$12.4 today · $3.10/hr · opus-5 62% · sonnet-5 38% · cache 96%"`, or
    /// `nil` when no row carries a measurement.
    ///
    /// `nil` is the whole point of the property: against an older `tcr`, or an
    /// offline read, there is nothing to say about spend and the header draws
    /// no line at all. A line of zeros would answer a question this build
    /// cannot answer.
    ///
    /// Two models are named and the rest collapse to `"+N"`: the panel is
    /// 380pt wide, and the third model's share is not what anyone opens it for.
    /// An unpriced model is named with `"?"` where its percentage would go and
    /// counts toward that `+N` like any other — see ``todayModelShare``.
    /// `" · N unpriced"` is appended whenever requests are missing from the
    /// cost, so a partial total says it is partial rather than passing itself
    /// off as the whole.
    ///
    /// The `/hr` segment is DROPPED, not printed as `n/a`, when the hour could
    /// not be priced: `"n/a/hr"` runs two tokens together and parses as a path,
    /// where every other `n/a` in this family stands alone as a word. The
    /// `N unpriced` clause already says why the rate is missing. An hour that
    /// served nothing is `$0.00/hr` — a measurement, not an absence.
    public var usageSummaryLine: String? {
        guard hasUsage else { return nil }
        var parts = ["\(QuotaFormat.usd(todayCost)) today"]
        if let hourly = lastHourCost {
            parts.append("\(QuotaFormat.usd(hourly))/hr")
        }
        let share = todayModelShare
        for entry in share.prefix(2) {
            parts.append("\(entry.model) \(entry.share.map { QuotaFormat.percent($0) } ?? "?")")
        }
        if share.count > 2 { parts.append("+\(share.count - 2)") }
        parts.append("cache \(QuotaFormat.percent(todayCacheHitRatio))")
        if let unpriced = todayUnpricedRequests, unpriced > 0 {
            parts.append("\(unpriced) unpriced")
        }
        return parts.joined(separator: " · ")
    }

}
