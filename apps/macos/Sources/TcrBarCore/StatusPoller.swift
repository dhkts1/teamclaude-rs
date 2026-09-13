import Combine
import Foundation

/// What the last poll established. Every failure mode is a distinct case with its
/// own text: a menu that silently shows an empty list when `tcr` is missing, or
/// when the proxy is down, is a defect — those are different facts and the
/// operator has to be able to tell them apart.
public enum PollState: Equatable {
    /// No poll has completed yet.
    case pending
    /// A fleet was decoded. `source` distinguishes measured counters from
    /// structural zeros.
    case loaded(Fleet)
    /// `tcr` is not on `PATH` and not in any known install directory.
    case toolMissing(searched: [String])
    /// `tcr status --json` exited non-zero — the usual cause is no server.
    case commandFailed(exitCode: Int32, message: String)
    /// The command succeeded but the payload did not match the expected shape.
    case undecodable(message: String)

    public var isHealthyRead: Bool {
        if case .loaded = self { return true }
        return false
    }

    /// One line, always non-empty, safe to put in front of a human.
    public var summary: String {
        switch self {
        case .pending:
            return "Waiting for first poll…"
        case .loaded(let fleet):
            let n = fleet.accounts.count
            let noun = n == 1 ? "account" : "accounts"
            // Every row failed to decode. Saying "0 accounts" here would read as
            // "you have none configured", which is a different and much calmer
            // fact than "tcr answered and this build could not read any of it".
            if n == 0, let unreadable = fleet.unreadableNotice {
                return "no account decoded — \(unreadable)"
            }
            let base =
                fleet.source.countersAreStructural
                ? "\(n) \(noun) — offline read, counters are structurally zero"
                : "\(n) \(noun) — live"
            guard let unreadable = fleet.unreadableNotice else { return base }
            return "\(base) · \(unreadable)"
        case .toolMissing(let searched):
            return "tcr not found on PATH (searched \(searched.count) locations)"
        case .commandFailed(let code, let message):
            let detail = message.isEmpty ? "no output" : message
            return "tcr status failed (exit \(code)): \(detail)"
        case .undecodable:
            // Deliberately drops `message`. It is a Swift `DecodingError`
            // description — key paths and type names — and this property's
            // contract one screen up is "safe to put in front of a human".
            // The raw text is still reachable: `FleetView` hangs it on the
            // banner's tooltip, which is where a reader who wants it looks.
            return "tcr answered, and this build could not read the answer"
        }
    }

    /// The `ready/enabled` label, e.g. `"9/13"` — opt-in, drawn only when the
    /// counts preference is on; the cup's fill level (``capacityFraction``)
    /// carries the same fact on the glyph itself by default.
    ///
    /// `nil` for anything but a healthy read of a fleet with at least one
    /// enabled account: the cup's colour and fill already carry "pending",
    /// "tool missing" and "poll failed" (``MenuBarShell/cupTint(for:awake:)``),
    /// and an all-disabled fleet has no numerator/denominator worth showing —
    /// `0/0` would read as a fault, not a fact, the same reasoning
    /// ``Fleet/countsSentence`` gives for returning `nil` in the identical
    /// case.
    public var countsLabel: String? {
        guard case .loaded(let fleet) = self, !fleet.enabledAccounts.isEmpty else { return nil }
        return "\(fleet.readyCount)/\(fleet.enabledCount)"
    }

    /// The cup's fill level: `readyCount / enabledCount`, `0...1`.
    ///
    /// `nil` when there is nothing to divide by — a poll that has not loaded a
    /// fleet yet, a read failure, or a fleet with no enabled accounts — the
    /// same condition ``countsLabel`` already treats as "nothing to show", so
    /// the two never disagree about which fleets have a real ratio. The mark
    /// draws `nil` as an empty cup (`MenuBarMark.image(fraction:tint:)`), never
    /// a `0` a reader could mistake for a measured "all spent".
    public var capacityFraction: Double? {
        guard case .loaded(let fleet) = self, fleet.enabledCount > 0 else { return nil }
        return Double(fleet.readyCount) / Double(fleet.enabledCount)
    }

    /// The menu-bar tooltip's own line: ``Fleet/countsSentence`` when there is
    /// one to give, else ``summary`` unchanged.
    ///
    /// `countsSentence` is `nil` for every case but a healthy read with at
    /// least one enabled account — a pending poll, a missing `tcr`, a failed
    /// command, an undecodable payload, and a healthy read of an all-disabled
    /// fleet all fall through to the same `summary` a reader already knows,
    /// rather than growing a second empty-fleet sentence to keep in sync with
    /// the first.
    public var tooltipSentence: String {
        guard case .loaded(let fleet) = self, let sentence = fleet.countsSentence else {
            return summary
        }
        guard let unreadable = fleet.unreadableNotice else { return sentence }
        return "\(sentence) · \(unreadable)"
    }

    /// The running-tools segment count, or `nil` when it must not be shown.
    ///
    /// Two independent gates, both required (`docs/design/menubar-mark-mockup.html`
    /// Rules: "appears only when the preference is on AND the wire carries
    /// `sessions`"): `showRunningTools` is the Settings toggle, and
    /// ``Fleet/sessionsSupported`` is whether THIS read actually populated
    /// ``Fleet/sessions`` — distinct from an empty ``Fleet/toolsRunning``, which
    /// is also true of a fleet with nothing running right now. A preference left
    /// on for months must never draw a false `0` while the wire that would fill
    /// it in does not exist yet (see ``Fleet/sessions``'s own doc-comment).
    public func runningToolsCount(showRunningTools: Bool) -> Int? {
        guard showRunningTools, case .loaded(let fleet) = self, fleet.sessionsSupported else {
            return nil
        }
        return fleet.toolsRunning.count
    }

    /// Whether the `ready/enabled` count should draw amber: zero accounts ready
    /// and at least one near its limit. Reuses ``Fleet/capacityGlyphState``'s own
    /// `.near` case rather than a second predicate that could drift from the
    /// glyph's shape — the glyph and the count's colour must always agree on
    /// which state they are both describing.
    public var countIsNearCapacity: Bool {
        guard case .loaded(let fleet) = self else { return false }
        return fleet.capacityGlyphState == .near
    }

    /// Which colour REGIME the one coffee-cup glyph is in — the part of
    /// ``MenuBarMark/Tint`` that does not need an actual `NSColor` to decide,
    /// so it can live (and be tested) here in `TcrBarCore` rather than in
    /// `MenuBarShell`, which the test target does not link (see
    /// `RunningToolsMarkTests`'s own doc-comment on that boundary). The
    /// caller (`MenuBarShell.cupTint(for:awake:)`) turns this into a real
    /// ``MenuBarMark/Tint`` by attaching `Tok`'s colours.
    ///
    /// Precedence, highest first:
    ///
    ///  1. A read failure (`toolMissing`, `commandFailed`, `undecodable`) is
    ///     `.failed` — there is no fleet to ask about capacity or keep-awake
    ///     at all, so this outranks everything else.
    ///  2. `Fleet.capacityGlyphState == .near` is `.near` — fleet *capacity*,
    ///     not the worst account: in a rotating pool spent accounts are the
    ///     mechanism working, so a worst-wins colour would sit at its most
    ///     alarming setting whenever any one of thirteen accounts was spent,
    ///     which is nearly always. Reuses the identical predicate
    ///     ``countIsNearCapacity`` does, so the two can never disagree about
    ///     which state they are both describing.
    ///  3. Otherwise: `.awake` while keep-awake holds the Mac up, else
    ///     `.template` — the plain, system-tinted cup.
    ///
    /// `.pending` and an all-disabled fleet both fall through to step 3:
    /// neither is a failure, so the cup stays whatever keep-awake says while
    /// its fill level (``capacityFraction``) draws empty.
    public enum CupTintKind: Equatable {
        case template
        case awake
        case near
        case failed
    }

    public func capacityTintKind(awake: Bool) -> CupTintKind {
        switch self {
        case .toolMissing, .commandFailed, .undecodable:
            return .failed
        case .loaded(let fleet) where fleet.capacityGlyphState == .near:
            return .near
        case .pending, .loaded:
            return awake ? .awake : .template
        }
    }
}

/// Runs `tcr status --json` on a timer and publishes the result.
@MainActor
public final class StatusPoller: ObservableObject {
    /// 3s: fast enough that a quota flip is visible while watching, slow enough
    /// that the CLI's own work stays in the noise.
    public nonisolated static let defaultInterval: TimeInterval = 3

    @Published public private(set) var state: PollState = .pending
    @Published public private(set) var lastPollAt: Date?

    public let interval: TimeInterval
    private var task: Task<Void, Never>?

    public init(interval: TimeInterval = StatusPoller.defaultInterval) {
        self.interval = interval
    }

    /// A poller pinned to one state, for deterministic rendering.
    ///
    /// The state-rendering harness and SwiftUI previews both need a panel that
    /// shows a chosen state without running `tcr` or starting a timer. Without
    /// this seam the only way to see a state is to make the real fleet enter it,
    /// which for "unreadable row" or "zero capacity" means waiting for a bad day.
    ///
    /// It never calls `start()`, so no timer exists and nothing is executed.
    public init(pinnedState: PollState, lastPollAt: Date? = nil) {
        self.interval = Self.defaultInterval
        self.state = pinnedState
        self.lastPollAt = lastPollAt
    }

    deinit { task?.cancel() }

    public func start() {
        guard task == nil else { return }
        task = Task { [weak self] in
            guard let self else { return }
            while !Task.isCancelled {
                await self.pollOnce()
                try? await Task.sleep(nanoseconds: UInt64(self.interval * 1_000_000_000))
            }
        }
    }

    public func stop() {
        task?.cancel()
        task = nil
    }

    /// Returns the state it just published, so a caller that polls *in order to
    /// check something* can compare against the exact read it triggered rather
    /// than against whatever `state` holds by the time it looks — a later timer
    /// tick can land in between. The toggle read-back
    /// (``AccountController/record(readback:requestedEnabled:account:now:)``)
    /// depends on that.
    @discardableResult
    public func pollOnce() async -> PollState {
        let next = await Task.detached(priority: .utility) { Self.fetch() }.value
        state = next
        lastPollAt = Date()
        return next
    }

    /// Blocking fetch — always called off the main actor.
    nonisolated static func fetch() -> PollState {
        switch TcrTool.resolve() {
        case .failure(let notFound):
            return .toolMissing(searched: notFound.searched)
        case .success(let executable):
            do {
                let output = try TcrTool.run(executable: executable, arguments: ["status", "--json"])
                return classify(output)
            } catch {
                return .commandFailed(exitCode: -1, message: error.localizedDescription)
            }
        }
    }

    /// Pure classification of a finished invocation — the part worth testing.
    public nonisolated static func classify(_ output: TcrTool.Output) -> PollState {
        guard output.exitCode == 0 else {
            return .commandFailed(
                exitCode: output.exitCode,
                message: output.stderr.trimmingCharacters(in: .whitespacesAndNewlines)
            )
        }
        do {
            return .loaded(try Fleet.decode(output.stdout))
        } catch {
            return .undecodable(message: "\(error)")
        }
    }
}
