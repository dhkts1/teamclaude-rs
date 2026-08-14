import Combine
import Foundation

/// Assigning and clearing the identity-bound control account.
///
/// Mirrors ``AccountControl``'s two rules exactly — this is the same subprocess
/// shape with a different verb, not a new pattern:
///
///  1. **This app never writes the tcr config.** The assignment is a subprocess —
///     `tcr control <name>` / `tcr control --clear` — and this process stays
///     credential-free.
///  2. **Exit 0 with anything on stderr is not a clean success.** The running
///     proxy being too old for the `control` route, or the account name failing
///     to resolve on the Rust side's exact-match rules, both arrive on stderr
///     while the process still exits 0. The UI never optimistically flips its
///     own copy of `control` — it re-polls and shows whatever `tcr status` then
///     reports.
public enum ControlCommand {
    /// The complete argument vector. `name` is passed positionally and
    /// verbatim — no `--org`, no flags, nothing that could name a process.
    public static func arguments(name: String) -> [String] {
        ["control", name]
    }

    /// The argument vector for clearing the control account entirely.
    public static func clearArguments() -> [String] {
        ["control", "--clear"]
    }

    /// Why an assignment or clear did not happen. Carries the CLI's own words;
    /// this app does not paraphrase `tcr`'s failures.
    public struct Failure: Error, Equatable, Sendable {
        public let assigning: Bool
        public let exitCode: Int32
        public let message: String

        public init(assigning: Bool, exitCode: Int32, message: String) {
            self.assigning = assigning
            self.exitCode = exitCode
            self.message = message
        }

        /// One line, always non-empty, safe to render in a row.
        public var summary: String {
            let verb = assigning ? "set control account" : "clear control account"
            let detail = message.isEmpty ? "no output" : message
            return "\(verb) failed (exit \(exitCode)): \(detail)"
        }
    }

    /// What a finished invocation was. Three arms, because exit 0 has two
    /// meanings: `tcr` either said nothing or said something, and the second is
    /// not a clean success (rule 2 above).
    public enum Outcome: Equatable, Sendable {
        /// Exit 0, nothing on stderr. The only outcome that may end in a bare `✓`.
        case clean
        /// Exit 0 and `tcr` wrote to stderr. `notice` is those bytes, trimmed at
        /// the ends and otherwise verbatim.
        case spoke(notice: String)
        /// A non-zero exit, or this app failing to run `tcr` at all.
        case failed(Failure)
    }

    /// Pure classification of a finished invocation.
    public static func classify(assigning: Bool, exitCode: Int32, stderr: String) -> Outcome {
        let text = stderr.trimmingCharacters(in: .whitespacesAndNewlines)
        guard exitCode == 0 else {
            return .failed(Failure(assigning: assigning, exitCode: exitCode, message: text))
        }
        return text.isEmpty ? .clean : .spoke(notice: text)
    }

    /// Blocking invocation — always called off the main actor.
    nonisolated static func perform(name: String?) -> Outcome {
        let assigning = name != nil
        switch TcrTool.resolve() {
        case .failure(let notFound):
            return .failed(
                Failure(
                    assigning: assigning,
                    exitCode: -1,
                    message: "tcr not found (searched \(notFound.searched.count) locations)"
                ))
        case .success(let executable):
            do {
                let output = try TcrTool.run(
                    executable: executable,
                    arguments: name.map(arguments(name:)) ?? clearArguments()
                )
                return classify(assigning: assigning, exitCode: output.exitCode, stderr: output.stderr)
            } catch {
                return .failed(
                    Failure(assigning: assigning, exitCode: -1, message: error.localizedDescription))
            }
        }
    }
}

/// Panel-facing state for the control-account assignment: which call is in
/// flight, and the last failure that has not been superseded by a later one.
@MainActor
public final class ControlController: ObservableObject {
    @Published public private(set) var pending: Bool = false
    @Published public private(set) var failure: ControlCommand.Failure?

    public init() {}

    /// Assign `name` as the control account, or pass `nil` to clear it.
    /// `.accepted` only when `tcr` exited 0 — the caller uses that to decide
    /// whether a status refresh is worth doing, never to update a local copy
    /// of `control`.
    @discardableResult
    public func setControl(_ name: String?) async -> AccountController.Attempt {
        guard !pending else { return .skipped }
        pending = true
        failure = nil
        defer { pending = false }

        let outcome = await Task.detached(priority: .userInitiated) {
            ControlCommand.perform(name: name)
        }.value

        switch outcome {
        case .clean:
            return .accepted(notice: nil)
        case .spoke(let notice):
            return .accepted(notice: notice)
        case .failed(let f):
            failure = f
            return .refused
        }
    }
}
