import Foundation

// Decoders for `tcr status --json`.
//
// The wire format is a *bare JSON array*, one object per account. Every field
// name here is the camelCase key the CLI emits (src/cli.rs, the `"quotaState":`
// block); the Rust side owns that spelling, so this file is the one place that
// has to track it.
//
// Two deliberate decisions:
//
//  1. Unknown enum payloads decode to an `.unknown(String)` case that carries the
//     raw text. A `quotaState` variant added on the Rust side must degrade to a
//     visible-but-unstyled label, never to a decode failure that blanks the panel.
//  2. `cacheHitRatio`, `probeError` and `lastStreamError` are genuinely `null` in
//     live output and are optional here. `cacheHitRatio` is null — never `0.0` —
//     when there is nothing to divide by, so an optional is the honest type: a
//     structural absence must not render as a measured zero.

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

    /// `"7d resets in 25h 28m"`. Minutes-only under an hour; never a bare number.
    public var countdownLabel: String {
        "\(window) resets in \(Self.duration(minutes: minutesUntilReset))"
    }

    static func duration(minutes: Int) -> String {
        if minutes <= 0 { return "now" }
        let hours = minutes / 60
        let rest = minutes % 60
        if hours == 0 { return "\(rest)m" }
        if rest == 0 { return "\(hours)h" }
        return "\(hours)h \(rest)m"
    }
}

/// One account row of `tcr status --json`.
public struct Account: Decodable, Equatable, Identifiable, Sendable {
    public let name: String
    public let priority: Int
    /// Free-form on the Rust side; displayed verbatim, never pattern-matched.
    public let status: String
    public let disabled: Bool

    public let quota: Double
    public let quotaState: QuotaState
    public let fiveHour: Double
    public let sevenDay: Double
    public let sevenDayOi: Double
    public let held: [HeldWindow]

    public let requests: Int
    public let inputTokens: Int
    public let outputTokens: Int
    public let cacheReadTokens: Int
    /// `null` when `inputTokens` is 0 — an absence, not a measured zero.
    public let cacheHitRatio: Double?

    public let probeStatus: String
    public let probeError: String?
    public let lastStreamError: String?
    public let streamErrorCount: Int

    public let source: StatusSource
    public let serverSha: String?
    public let serverDirty: Bool?

    public var id: String { name }

    /// Worst-first ordering key for the fleet glyph. A disabled account is not an
    /// alarm — it is an operator decision — so it sorts below a spent one.
    public var severity: Int {
        if disabled { return 1 }
        return quotaState.severity + 1
    }

    /// The window with the soonest reset, when the account is held.
    public var soonestHold: HeldWindow? {
        held.min(by: { $0.minutesUntilReset < $1.minutesUntilReset })
    }
}

/// The decoded fleet, plus the facts that are properties of the *fetch* rather
/// than of any one account.
public struct Fleet: Equatable, Sendable {
    public let accounts: [Account]

    public init(accounts: [Account]) {
        self.accounts = accounts
    }

    public static func decode(_ data: Data) throws -> Fleet {
        Fleet(accounts: try JSONDecoder().decode([Account].self, from: data))
    }

    /// Every row carries the same `source`; take the first and say so.
    public var source: StatusSource { accounts.first?.source ?? .unknown("none") }

    public var serverSha: String? { accounts.first?.serverSha }
    public var serverDirty: Bool { accounts.first?.serverDirty ?? false }

    /// The account driving the menu-bar glyph.
    public var worst: Account? {
        accounts.max(by: { $0.severity < $1.severity })
    }

    public var enabledAccounts: [Account] { accounts.filter { !$0.disabled } }

    /// Fleet-wide headline state, worst-account-wins.
    public var headline: QuotaState {
        enabledAccounts.map(\.quotaState).max(by: { $0.severity < $1.severity }) ?? .unknown("empty")
    }
}
