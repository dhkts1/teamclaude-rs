import Combine
import Foundation

/// Enabling and disabling a single account.
///
/// Two rules shape everything here.
///
///  1. **This app never writes the tcr config.** `~/.config/teamclaude.json` holds
///     OAuth access and refresh tokens; `tcr` owns that file and is the only thing
///     that edits it. So the toggle is a subprocess — `tcr enable <name>` /
///     `tcr disable <name>` — exactly like the server control, and this process
///     stays credential-free.
///  2. **A failed call must never look like a success.** `query` on the Rust side
///     (`src/main.rs`, `EnableArgs` / `DisableArgs`) is a *case-insensitive
///     substring* match, so passing an exact account name is still not a guarantee
///     of a unique hit: if one configured name is a substring of another, `tcr` can
///     refuse as ambiguous, or resolve to the wrong account. The exit code and
///     stderr are therefore captured and surfaced in the row, and the UI never
///     optimistically flips its own copy of `disabled` — it re-polls and shows
///     whatever `tcr status` then reports.
///
/// What this does **not** claim: that a running proxy observes the change
/// immediately. `tcr` rewrites the config file; whether the live server re-reads it
/// without a restart is **unverified** in either direction from this app's side.
/// The panel therefore only ever asserts what `tcr status` reports, which is the
/// config-level fact.
public enum AccountCommand {
    /// The complete argument vector. `name` is passed positionally and verbatim —
    /// no `--org`, no flags, nothing that could name a process.
    ///
    /// `enabled: true` means "put this account back in rotation".
    public static func arguments(enabled: Bool, name: String) -> [String] {
        [enabled ? "enable" : "disable", name]
    }

    /// Why a toggle did not happen. Carries the CLI's own words; this app does not
    /// paraphrase `tcr`'s failures.
    public struct Failure: Error, Equatable, Sendable {
        public let enabling: Bool
        public let exitCode: Int32
        public let message: String

        public init(enabling: Bool, exitCode: Int32, message: String) {
            self.enabling = enabling
            self.exitCode = exitCode
            self.message = message
        }

        /// One line, always non-empty, safe to render in a row.
        public var summary: String {
            let verb = enabling ? "enable" : "disable"
            let detail = message.isEmpty ? "no output" : message
            return "\(verb) failed (exit \(exitCode)): \(detail)"
        }
    }

    /// Pure classification of a finished invocation. `nil` is success.
    public static func classify(enabling: Bool, exitCode: Int32, stderr: String) -> Failure? {
        guard exitCode != 0 else { return nil }
        return Failure(
            enabling: enabling,
            exitCode: exitCode,
            message: stderr.trimmingCharacters(in: .whitespacesAndNewlines)
        )
    }

    /// Blocking invocation — always called off the main actor.
    nonisolated static func perform(enabled: Bool, name: String) -> Failure? {
        switch TcrTool.resolve() {
        case .failure(let notFound):
            return Failure(
                enabling: enabled,
                exitCode: -1,
                message: "tcr not found (searched \(notFound.searched.count) locations)"
            )
        case .success(let executable):
            do {
                let output = try TcrTool.run(
                    executable: executable,
                    arguments: arguments(enabled: enabled, name: name)
                )
                return classify(enabling: enabled, exitCode: output.exitCode, stderr: output.stderr)
            } catch {
                return Failure(enabling: enabled, exitCode: -1, message: error.localizedDescription)
            }
        }
    }
}

/// Per-account toggle state for the panel: which rows have a call in flight, and
/// which rows have a failure that has not been superseded by a later attempt.
///
/// Keyed by account name, which is `Account.id`.
@MainActor
public final class AccountController: ObservableObject {
    @Published public private(set) var pending: Set<String> = []
    @Published public private(set) var failures: [String: AccountCommand.Failure] = [:]

    public init() {}

    public func isPending(_ name: String) -> Bool { pending.contains(name) }
    public func failure(for name: String) -> AccountCommand.Failure? { failures[name] }

    /// Run the toggle. Returns `true` only when `tcr` exited 0 — the caller uses
    /// that to decide whether a status refresh is worth doing, never to update a
    /// local copy of `disabled`.
    @discardableResult
    public func setEnabled(_ enabled: Bool, account name: String) async -> Bool {
        guard !pending.contains(name) else { return false }
        pending.insert(name)
        // A new attempt clears the previous verdict; a stale error beside a
        // now-succeeding row would be its own lie.
        failures[name] = nil
        defer { pending.remove(name) }

        let failure = await Task.detached(priority: .userInitiated) {
            AccountCommand.perform(enabled: enabled, name: name)
        }.value

        guard let failure else { return true }
        failures[name] = failure
        return false
    }
}
