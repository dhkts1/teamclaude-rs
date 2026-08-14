import Combine
import Foundation

/// Setting, clearing, and reading the identity-bound **control account** — the
/// one account `tcr control` holds out of the inference rotation while still
/// tracking its usage (`src/main.rs`'s `Control` subcommand; see its own
/// doc-comment for the exact CLI contract this mirrors).
///
/// Same two rules as ``AccountController``/``AccountCommand``, and for the same
/// reasons:
///
///  1. **This app never writes the tcr config.** Every change is a subprocess —
///     `tcr control <query>` / `tcr control --clear` — never a direct edit of
///     `~/.config/teamclaude.json`.
///  2. **This app never depends on `tcr status --json` for this fact.** The
///     brief for this feature is explicit that whether that payload carries a
///     `control` field is a separate, undecided question; reading it here would
///     couple this UI to that decision. `tcr control --show` is its own
///     read path and stays that way regardless of what `status` does later.
///
/// A third rule is new to this controller, because `--show` is a command that
/// can fail in a way `tcr status` cannot: an **older `tcr`** — the common case
/// while this feature is unmerged on the Rust side — has no `control`
/// subcommand at all and exits non-zero with "unrecognized subcommand". That is
/// not "no control account is set"; it is "this build cannot answer the
/// question", and the two must never collapse into the same `nil`. See
/// ``current`` and ``unavailable``.
public enum ControlAccountCommand {
    /// `tcr control --show`.
    public static let showArguments = ["control", "--show"]

    /// `tcr control <name>` to set it, or `tcr control --clear` to clear it.
    /// `name` is passed positionally and verbatim, exactly like
    /// ``AccountCommand/arguments(enabled:name:)`` — no `--org`, nothing that
    /// could be mistaken for a flag.
    public static func setArguments(name: String?) -> [String] {
        guard let name else { return ["control", "--clear"] }
        return ["control", name]
    }

    /// What `--show` established. Three arms, not two — mirrors
    /// ``PollState`` in keeping "no reading" and "a reading of none" apart.
    public enum Reading: Equatable, Sendable {
        /// A control account is set, by name.
        case set(String)
        /// `tcr control --show` ran and printed `(none)`: this build asked and
        /// the answer is that nothing is set.
        case none
        /// The question could not be asked — `tcr` is missing, the subcommand
        /// does not exist on this build, or the call otherwise failed. Distinct
        /// from ``none`` on purpose: rendering this as "no control account" would
        /// be the same false-negative `PollState.toolMissing` was split out to
        /// avoid for the fleet list.
        case unavailable(reason: String)
    }

    /// Pure classification of a finished `--show` invocation.
    public static func classifyShow(exitCode: Int32, stdout: Data, stderr: String) -> Reading {
        guard exitCode == 0 else {
            let text = stderr.trimmingCharacters(in: .whitespacesAndNewlines)
            return .unavailable(reason: text.isEmpty ? "exit \(exitCode)" : text)
        }
        let text =
            String(data: stdout, encoding: .utf8)?
            .trimmingCharacters(in: .whitespacesAndNewlines) ?? ""
        if text.isEmpty || text == "(none)" { return .none }
        return .set(text)
    }

    /// Why a set/clear did not happen. `tcr`'s own words, unparaphrased — same
    /// posture as ``AccountCommand/Failure``.
    public struct Failure: Error, Equatable, Sendable {
        public let exitCode: Int32
        public let message: String

        public init(exitCode: Int32, message: String) {
            self.exitCode = exitCode
            self.message = message
        }

        public var summary: String {
            let detail = message.isEmpty ? "no output" : message
            return "control failed (exit \(exitCode)): \(detail)"
        }
    }

    /// Exit 0 has two meanings here too — `tcr` can apply the change to the
    /// file only and warn on stderr that the running proxy was too old for the
    /// route (`src/cli.rs`'s `set_control`, the `NoRoute` arm) — so this stays a
    /// three-arm outcome exactly like ``AccountCommand/Outcome``.
    public enum Outcome: Equatable, Sendable {
        case clean
        case spoke(notice: String)
        case failed(Failure)
    }

    public static func classifySet(exitCode: Int32, stderr: String) -> Outcome {
        let text = stderr.trimmingCharacters(in: .whitespacesAndNewlines)
        guard exitCode == 0 else {
            return .failed(Failure(exitCode: exitCode, message: text))
        }
        return text.isEmpty ? .clean : .spoke(notice: text)
    }

    /// Blocking `--show` — always called off the main actor.
    nonisolated static func performShow() -> Reading {
        switch TcrTool.resolve() {
        case .failure(let notFound):
            return .unavailable(
                reason: "tcr not found (searched \(notFound.searched.count) locations)")
        case .success(let executable):
            do {
                let output = try TcrTool.run(executable: executable, arguments: showArguments)
                return classifyShow(exitCode: output.exitCode, stdout: output.stdout, stderr: output.stderr)
            } catch {
                return .unavailable(reason: error.localizedDescription)
            }
        }
    }

    /// Blocking set/clear — always called off the main actor.
    nonisolated static func performSet(name: String?) -> Outcome {
        switch TcrTool.resolve() {
        case .failure(let notFound):
            return .failed(
                Failure(
                    exitCode: -1,
                    message: "tcr not found (searched \(notFound.searched.count) locations)"))
        case .success(let executable):
            do {
                let output = try TcrTool.run(
                    executable: executable, arguments: setArguments(name: name))
                return classifySet(exitCode: output.exitCode, stderr: output.stderr)
            } catch {
                return .failed(Failure(exitCode: -1, message: error.localizedDescription))
            }
        }
    }
}

/// Panel-facing state for the control account: which one is set (if this build
/// can tell), and per-account in-flight/failure tracking for the gear menu's
/// "Use as control account" / "Clear control account" actions.
@MainActor
public final class ControlAccountController: ObservableObject {
    /// The account name currently held as control, or `nil` when either none is
    /// set or this build cannot ask (see ``unavailable``). Never written
    /// optimistically by a set/clear call — only ``refresh()`` ever assigns it,
    /// so the row always shows what `tcr control --show` actually said, the
    /// same discipline ``AccountController`` uses for `disabled`.
    @Published public private(set) var current: String?
    /// True when the last `--show` could not answer the question at all — an
    /// older `tcr`, no binary found, or the call erroring. The row must degrade
    /// silently on this (no phantom checkmark, no error banner), per this
    /// feature's own brief.
    @Published public private(set) var unavailable: Bool = false
    @Published public private(set) var pending: Set<String> = []
    @Published public private(set) var failures: [String: ControlAccountCommand.Failure] = [:]

    public init() {}

    /// A controller pinned to one reading, for deterministic rendering — the
    /// same seam ``StatusPoller/init(pinnedState:lastPollAt:)`` provides for the
    /// fleet. It never calls `refresh()`, so no subprocess ever runs.
    public init(pinned: String?, unavailable: Bool = false) {
        self.current = pinned
        self.unavailable = unavailable
    }

    public func isPending(_ name: String) -> Bool { pending.contains(name) }
    public func failure(for name: String) -> ControlAccountCommand.Failure? { failures[name] }
    public func isControl(_ name: String) -> Bool { !unavailable && current == name }

    /// Re-read `tcr control --show`. Safe to call any time — on panel open,
    /// after a set/clear, or on a timer — and idempotent with itself; nothing
    /// here assumes it is the only caller.
    public func refresh() async {
        let reading = await Task.detached(priority: .utility) {
            ControlAccountCommand.performShow()
        }.value
        switch reading {
        case .set(let name):
            unavailable = false
            current = name
        case .none:
            unavailable = false
            current = nil
        case .unavailable:
            unavailable = true
            current = nil
        }
    }

    /// What a set/clear did, as far as the subprocess can say — mirrors
    /// ``AccountController/Attempt``.
    public enum Attempt: Equatable, Sendable {
        case skipped
        case refused
        case accepted(notice: String?)
    }

    /// Set `name` as the control account, or clear it when `name` is `nil`.
    /// `key` is the account name used to key ``pending``/``failures`` — for a
    /// clear it is the name of the row that currently holds the control, so
    /// that row (and only that row) shows the in-flight/failure state.
    ///
    /// `.accepted` only means `tcr` exited 0; the caller must follow it with
    /// ``refresh()`` to learn what actually took, exactly like
    /// ``AccountController/setEnabled(_:account:)`` never updates `disabled`
    /// itself.
    @discardableResult
    public func setControl(name: String?, key: String) async -> Attempt {
        guard !pending.contains(key) else { return .skipped }
        pending.insert(key)
        failures[key] = nil
        defer { pending.remove(key) }

        let outcome = await Task.detached(priority: .userInitiated) {
            ControlAccountCommand.performSet(name: name)
        }.value

        switch outcome {
        case .clean:
            return .accepted(notice: nil)
        case .spoke(let notice):
            return .accepted(notice: notice)
        case .failed(let failure):
            failures[key] = failure
            return .refused
        }
    }
}
