import Combine
import Foundation

/// Mutating group membership: adding an account to a group, removing one
/// from a group, or removing a whole group. Same two rules as
/// ``AccountCommand``/``ControlAccountCommand``, and for the same reasons:
///
///  1. **This app never writes the tcr config.** Every change is a
///     subprocess — `tcr group add <group> <account>`,
///     `tcr group rm <group> <account>`, `tcr group rm <group> --all` —
///     never a direct edit of `~/.config/teamclaude.json`. There is also no
///     live config reload: a successful call here changes nothing until the
///     proxy restarts, which the view surfaces, and this type never claims
///     otherwise.
///  2. **Exit 0 with anything on stderr is not a clean success.** Same
///     three-arm `Outcome` as ``AccountCommand``, for the same reason: `tcr`
///     can apply a change to the file only and warn that a running proxy was
///     too old for the control route.
public enum GroupCommand {
    /// `tcr group add <group> <account>`. Both arguments positional and
    /// verbatim — no flags, nothing that could be mistaken for one.
    public static func addArguments(group: String, account: String) -> [String] {
        ["group", "add", group, account]
    }

    /// `tcr group rm <group> <account>`.
    public static func removeArguments(group: String, account: String) -> [String] {
        ["group", "rm", group, account]
    }

    /// `tcr group rm <group> --all` — removes the whole group.
    public static func removeAllArguments(group: String) -> [String] {
        ["group", "rm", group, "--all"]
    }

    /// Why a group command did not happen. Carries `tcr`'s own words verbatim,
    /// same posture as ``AccountCommand/Failure``.
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
            return "group command failed (exit \(exitCode)): \(detail)"
        }
    }

    /// Mirrors ``AccountCommand/Outcome`` exactly, same three arms and same
    /// reason: exit 0 has two meanings.
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
    nonisolated static func perform(arguments: [String]) -> Outcome {
        switch TcrTool.resolve() {
        case .failure(let notFound):
            return .failed(
                Failure(
                    exitCode: -1,
                    message: "tcr not found (searched \(notFound.searched.count) locations)"
                ))
        case .success(let executable):
            do {
                let output = try TcrTool.run(executable: executable, arguments: arguments)
                return classify(exitCode: output.exitCode, stderr: output.stderr)
            } catch {
                return .failed(Failure(exitCode: -1, message: error.localizedDescription))
            }
        }
    }
}

/// Client-side validation for a typed group name, mirroring the rule the CLI
/// itself enforces so a bad name is caught before a subprocess ever runs
/// rather than surfacing as `tcr`'s stderr after the fact.
public enum GroupNameValidation {
    public enum Failure: Equatable, Sendable {
        /// Nothing typed, or only whitespace.
        case empty
        /// A C0 control character or DEL (`U+0000`-`U+001F`, `U+007F`).
        case controlCharacter
        /// A codepoint above `U+00FF` — the CLI's name charset is Latin-1.
        case aboveLatin1
    }

    /// `nil` means the name is acceptable.
    public static func validate(_ name: String) -> Failure? {
        let trimmed = name.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else { return .empty }
        for scalar in name.unicodeScalars {
            if scalar.value < 0x20 || scalar.value == 0x7F { return .controlCharacter }
            if scalar.value > 0xFF { return .aboveLatin1 }
        }
        return nil
    }

    /// One line, safe to render beside the text field.
    public static func message(for failure: Failure) -> String {
        switch failure {
        case .empty: return "Group name cannot be empty."
        case .controlCharacter: return "Group name cannot contain control characters."
        case .aboveLatin1: return "Group name cannot contain characters above U+00FF."
        }
    }
}

/// Panel-facing state for group mutations: which calls are in flight, and
/// which have a failure that has not been superseded by a later attempt.
/// Mirrors ``AccountController``'s shape.
///
/// Keyed by a caller-chosen string rather than by account name alone —
/// `"<group>/<account>"` for a member add/remove, the bare group name for a
/// whole-group removal — so two different members of the same group, or an
/// add and a remove on two different rows, never share in-flight/failure
/// state.
@MainActor
public final class GroupController: ObservableObject {
    @Published public private(set) var pending: Set<String> = []
    @Published public private(set) var failures: [String: GroupCommand.Failure] = [:]
    /// Group names that have had at least one successful mutation this
    /// session — drives the "restart the proxy to apply" note. Never
    /// cleared: there is no live config reload, so once true it stays true
    /// until the app relaunches, which matches reality.
    @Published public private(set) var appliedPendingRestart: Set<String> = []

    public init() {}

    public func isPending(_ key: String) -> Bool { pending.contains(key) }
    public func failure(for key: String) -> GroupCommand.Failure? { failures[key] }
    public func needsRestart(_ group: String) -> Bool { appliedPendingRestart.contains(group) }

    /// What a call did, as far as the subprocess can say — mirrors
    /// ``AccountController/Attempt``.
    public enum Attempt: Equatable, Sendable {
        case skipped
        case refused
        case accepted(notice: String?)
    }

    @discardableResult
    private func run(key: String, group: String, arguments: [String]) async -> Attempt {
        guard !pending.contains(key) else { return .skipped }
        pending.insert(key)
        failures[key] = nil
        defer { pending.remove(key) }

        let outcome = await Task.detached(priority: .userInitiated) {
            GroupCommand.perform(arguments: arguments)
        }.value

        switch outcome {
        case .clean:
            appliedPendingRestart.insert(group)
            return .accepted(notice: nil)
        case .spoke(let notice):
            appliedPendingRestart.insert(group)
            return .accepted(notice: notice)
        case .failed(let failure):
            failures[key] = failure
            return .refused
        }
    }

    /// `tcr group add <group> <account>`.
    @discardableResult
    public func add(account: String, to group: String) async -> Attempt {
        await run(
            key: "\(group)/\(account)", group: group,
            arguments: GroupCommand.addArguments(group: group, account: account))
    }

    /// `tcr group rm <group> <account>`.
    @discardableResult
    public func remove(account: String, from group: String) async -> Attempt {
        await run(
            key: "\(group)/\(account)", group: group,
            arguments: GroupCommand.removeArguments(group: group, account: account))
    }

    /// `tcr group rm <group> --all`.
    @discardableResult
    public func removeAll(group: String) async -> Attempt {
        await run(key: group, group: group, arguments: GroupCommand.removeAllArguments(group: group))
    }
}
