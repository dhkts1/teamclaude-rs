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
        case .undecodable(let message):
            return "tcr status returned unreadable output: \(message)"
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
