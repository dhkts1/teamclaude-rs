import Combine
import Foundation

/// Whether a group can actually serve traffic, as opposed to merely existing
/// with a member and a colour.
///
/// **Mirrors a proxy rule and must not drift from it.** The authority is
/// `Manager::classify_group_miss`'s `GroupMiss::OnlyControl` arm
/// (`src/manager/select.rs`): an inference pick excludes the control account
/// unconditionally, so a group with no other member can never be selected, and
/// every request asking for it is served from the whole pool instead. The `tcr`
/// CLI computes the same fact for `tcr group ls`'s `routes=` field; this is the
/// panel's copy, computed from data the panel already has (`Account.groups` plus
/// the name `ControlAccountController` resolves) rather than a new wire field.
///
/// Deliberately NOT a health check: a member that is merely disabled or
/// rate-limited right now still counts as routable, because that is transient and
/// this is about the group's permanent shape.
public enum GroupRouting {
    /// `false` when the group has no members at all, or when every member is the
    /// control account. `controlName == nil` means no control account is set (or
    /// the panel could not resolve one), in which case membership alone decides.
    public static func routes(group: String, accounts: [Account], controlName: String?) -> Bool {
        let members = accounts.filter { ($0.groups ?? []).contains(group) }
        guard !members.isEmpty else { return false }
        guard let controlName else { return true }
        return members.contains { $0.name != controlName }
    }
}

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

    /// `tcr run --group <group>` — start a Claude Code session that PREFERS this
    /// group. The only argv here that is not a mutation, and the only one a
    /// person actually wants off a group row: every other entry in this menu
    /// administers the label, and none of them used it.
    ///
    /// **Copy-only. Never pass this to ``perform(arguments:)``.** That helper
    /// runs argv to completion and reads its exit code; `tcr run` launches an
    /// interactive Claude Code session, so running it from a menubar app would
    /// hang the app on a process with no terminal. It exists to be rendered by
    /// ``CopyCommandMenuEntry`` and put on the pasteboard, nothing else.
    public static func runArguments(group: String) -> [String] {
        ["run", "--group", group]
    }

    /// Shell-quotes a single argument for a copy-pasteable command line.
    /// Left bare when it is already safe unquoted (an email like
    /// `alice@example.com` reads better that way) — otherwise wrapped in
    /// single quotes with any embedded single quote escaped the POSIX way.
    /// The function must not simply assume an account name is safe: this is
    /// the one place that decides, not the call site.
    public static func shellQuote(_ argument: String) -> String {
        let safe = argument.range(of: "^[A-Za-z0-9_.@%+=:,/-]+$", options: .regularExpression) != nil
        if safe { return argument }
        let escaped = argument.replacingOccurrences(of: "'", with: "'\\''")
        return "'\(escaped)'"
    }

    /// The full command line a user can paste into a shell, e.g.
    /// `tcr group rm dev henry2@example.com`. Built by quoting the SAME argv
    /// an actual call runs — ``removeArguments(group:account:)`` and friends
    /// — never formatted from the group/account strings a second time, so
    /// this text and the action sitting next to it in the menu can never
    /// drift apart.
    public static func commandLine(arguments: [String]) -> String {
        (["tcr"] + arguments).map(shellQuote).joined(separator: " ")
    }

    /// A menu item's title and the text it copies to the pasteboard, both
    /// derived from the same argv via ``commandLine(arguments:)`` — so the
    /// title can never say one thing and the clipboard hold another. Used
    /// for both the remove form (`Copy "tcr group rm dev alice@example.com"`)
    /// and the add form offered for a group the account is not yet in.
    public struct CopyCommandMenuEntry: Equatable, Sendable {
        /// What the menu item reads. Never truncated here — only the
        /// rendered menu is free to clip it visually; the underlying string
        /// stays complete so `copiedText` can be derived from it unambiguously.
        public let title: String
        /// What lands on the pasteboard when the item is chosen.
        public let copiedText: String

        public init(arguments: [String]) {
            let line = commandLine(arguments: arguments)
            self.title = "Copy “\(line)”"
            self.copiedText = line
        }
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

/// Whether a typed name can be used to CREATE a new group — the CLI's
/// character rule, plus one more check only this call site needs: since a
/// group exists only while some account carries its label, "create a group"
/// and "add this account to an existing group" are the same server call
/// (`GroupCommand.addArguments`). Typing a name that already names a group
/// must not be allowed to silently take that second path — the account would
/// join an existing group while the operator believes they made a new one.
public enum NewGroupName: Equatable, Sendable {
    case rejected(GroupNameValidation.Failure)
    case duplicate
    case valid(String)

    /// `existingGroups` is the fleet-wide set, not just this account's own —
    /// the whole point is to catch a name that already belongs to someone
    /// else's membership.
    public static func evaluate(_ raw: String, existingGroups: Set<String>) -> NewGroupName {
        if let failure = GroupNameValidation.validate(raw) {
            return .rejected(failure)
        }
        let trimmed = raw.trimmingCharacters(in: .whitespacesAndNewlines)
        if existingGroups.contains(trimmed) {
            return .duplicate
        }
        return .valid(trimmed)
    }

    /// `nil` when the name is usable; otherwise the reason, safe to render
    /// beside the text field.
    public var rejectionMessage: String? {
        switch self {
        case .rejected(let failure): return GroupNameValidation.message(for: failure)
        case .duplicate: return "A group named that already exists."
        case .valid: return nil
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
