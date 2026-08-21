import Combine
import Foundation

/// Deleting a single account from the config — `tcr remove <query> [--org
/// <org>]` (`src/cli.rs`'s `remove_account`, dispatched from `src/main.rs`'s
/// `run_remove`). Same shape as ``AccountCommand``/``GroupCommand``, and for
/// the same reasons:
///
///  1. **This app never writes the tcr config.** `~/.config/teamclaude.json`
///     holds OAuth access and refresh tokens; deleting an account is a
///     subprocess, never a direct edit.
///  2. **This is not reversible from the UI.** `remove_account` deletes the
///     matched entry outright — there is no `tcr un-remove`. Getting the
///     account back means `tcr login` from scratch, which is why the panel
///     confirms before calling this at all.
///  3. **This does not take effect in a running proxy.** The account list is
///     a boot-time snapshot (`CLAUDE.md`'s config-reload rule): `tcr remove`
///     changes the file and exits 0, but the row stays on screen, unchanged,
///     until the proxy restarts. ``RemoveAccountController`` tracks that a
///     removal landed so the panel can say so, and never claims the row is
///     actually gone.
///  4. **Exit 0 with anything on stderr is not a clean success.** Same
///     three-arm ``Outcome`` as ``AccountCommand``/``GroupCommand``.
public enum RemoveAccountCommand {
    /// `tcr remove <query>`, or `tcr remove <query> --org <org>` to narrow an
    /// ambiguous match. `query` is passed positionally and verbatim — no
    /// shell involved, so nothing here needs to escape it.
    public static func arguments(query: String, org: String? = nil) -> [String] {
        guard let org else { return ["remove", query] }
        return ["remove", query, "--org", org]
    }

    /// Why a delete did not happen. `tcr`'s own words, unparaphrased.
    public struct Failure: Error, Equatable, Sendable {
        public let exitCode: Int32
        public let message: String

        public init(exitCode: Int32, message: String) {
            self.exitCode = exitCode
            self.message = message
        }

        /// One line, always non-empty, safe to render in the row.
        public var summary: String {
            let detail = message.isEmpty ? "no output" : message
            return "delete failed (exit \(exitCode)): \(detail)"
        }
    }

    /// Mirrors ``AccountCommand/Outcome``/``GroupCommand/Outcome`` exactly,
    /// same three arms and same reason: exit 0 has two meanings.
    public enum Outcome: Equatable, Sendable {
        case clean
        case spoke(notice: String)
        case failed(Failure)
    }

    /// Pure classification of a finished invocation.
    public static func classify(exitCode: Int32, stderr: String) -> Outcome {
        let text = stderr.trimmingCharacters(in: .whitespacesAndNewlines)
        guard exitCode == 0 else {
            return .failed(Failure(exitCode: exitCode, message: text))
        }
        return text.isEmpty ? .clean : .spoke(notice: text)
    }

    /// Blocking invocation — always called off the main actor.
    nonisolated static func perform(query: String, org: String? = nil) -> Outcome {
        switch TcrTool.resolve() {
        case .failure(let notFound):
            return .failed(
                Failure(
                    exitCode: -1,
                    message: "tcr not found (searched \(notFound.searched.count) locations)"
                ))
        case .success(let executable):
            do {
                let output = try TcrTool.run(
                    executable: executable, arguments: arguments(query: query, org: org))
                return classify(exitCode: output.exitCode, stderr: output.stderr)
            } catch {
                return .failed(Failure(exitCode: -1, message: error.localizedDescription))
            }
        }
    }
}

/// Panel-facing state for account deletion: which calls are in flight, which
/// have an unsuperseded failure, and which accounts have been successfully
/// deleted this session — the last of which drives the "restart the proxy to
/// apply" notice, since the row itself has no way to reflect a boot-time
/// config change on its own.
///
/// Keyed by account name, same as ``AccountController``. Never cleared: like
/// ``GroupController/appliedPendingRestart``, there is no live config reload
/// for this field, so once a delete lands it stays pending-restart until the
/// app relaunches (which matches reality) — the panel is expected to keep
/// rendering the notice on that row for as long as it is open.
@MainActor
public final class RemoveAccountController: ObservableObject {
    @Published public private(set) var pending: Set<String> = []
    @Published public private(set) var failures: [String: RemoveAccountCommand.Failure] = [:]
    @Published public private(set) var removed: Set<String> = []

    public init() {}

    public func isPending(_ name: String) -> Bool { pending.contains(name) }
    public func failure(for name: String) -> RemoveAccountCommand.Failure? { failures[name] }
    /// Whether `name` was successfully deleted from the config this session —
    /// the fact the "restart to apply" notice is drawn from.
    public func needsRestart(_ name: String) -> Bool { removed.contains(name) }

    /// What a call did, as far as the subprocess can say — mirrors
    /// ``AccountController/Attempt``/``GroupController/Attempt``.
    public enum Attempt: Equatable, Sendable {
        case skipped
        case refused
        case accepted(notice: String?)
    }

    /// `tcr remove <name>`. `org` narrows an ambiguous match, mirroring the
    /// CLI's own `--org` flag; the panel does not currently offer a way to
    /// set it, so callers pass `nil` until that becomes a real ambiguity to
    /// solve.
    @discardableResult
    public func remove(account name: String, org: String? = nil) async -> Attempt {
        guard !pending.contains(name) else { return .skipped }
        pending.insert(name)
        failures[name] = nil
        defer { pending.remove(name) }

        let outcome = await Task.detached(priority: .userInitiated) {
            RemoveAccountCommand.perform(query: name, org: org)
        }.value

        switch outcome {
        case .clean:
            removed.insert(name)
            return .accepted(notice: nil)
        case .spoke(let notice):
            removed.insert(name)
            return .accepted(notice: notice)
        case .failed(let failure):
            failures[name] = failure
            return .refused
        }
    }
}
