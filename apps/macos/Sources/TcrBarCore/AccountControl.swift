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
///     (`src/identity.rs`, `match_accounts`) is an EXACT match on the account name,
///     falling back to an exact match on the email part — case-sensitive `==` both
///     times, no substring anywhere. Passing the row's own `name` therefore resolves
///     to that row or to nothing, except where two accounts share an email across
///     orgs, which `match_one` returns as ambiguous rather than picking one. The
///     exit code and stderr are still captured and surfaced in the row, and the UI
///     never optimistically flips its own copy of `disabled` — it re-polls and
///     shows whatever `tcr status` then reports.
///  3. **Exit 0 with anything on stderr is not a clean success.** `tcr` reports
///     durability on the success path: it exits 0 having parked the live rotation
///     and warns on stderr that no config entry matched, so the account returns to
///     rotation on restart (`src/cli.rs`, the `warning` on
///     `SetDisabledOutcome::Applied`), or that the running proxy was too old for
///     the control route and only the file changed. Reading only the exit code
///     dropped both at the process boundary, and the row stamped `parked ✓` on a
///     change that would not survive a restart. So stderr is captured on the
///     success path too and carried, verbatim and un-parsed, into
///     ``ToggleVerdict/spokeUp(notice:about:)``.
///
/// What this does **not** claim: that a running proxy observes the change
/// immediately. `tcr` rewrites the config file; a live server that read `disabled`
/// once at boot keeps serving the old value, and `tcr status` prefers the live
/// server — so the re-poll can come back reporting the state that was just
/// changed away from. That is a real, measured outcome on this fleet, not a
/// theoretical one, and it is now RENDERED rather than assumed away: see
/// ``ToggleVerdict``. The panel still only ever asserts what `tcr status`
/// reports; it just no longer treats exit 0 as evidence that anything happened.
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

    /// What a finished invocation was. Three arms, because exit 0 has two
    /// meanings: `tcr` either said nothing or said something, and the second is
    /// not a clean success (rule 3 above).
    public enum Outcome: Equatable, Sendable {
        /// Exit 0, nothing on stderr. The only outcome that may end in a bare `✓`.
        case clean
        /// Exit 0 and `tcr` wrote to stderr. `notice` is those bytes, trimmed at
        /// the ends and otherwise verbatim — never matched against a phrase, so a
        /// warning added to `tcr` next year reaches the row without a change here.
        case spoke(notice: String)
        /// A non-zero exit, or this app failing to run `tcr` at all.
        case failed(Failure)
    }

    /// Pure classification of a finished invocation.
    public static func classify(enabling: Bool, exitCode: Int32, stderr: String) -> Outcome {
        let text = stderr.trimmingCharacters(in: .whitespacesAndNewlines)
        guard exitCode == 0 else {
            return .failed(Failure(enabling: enabling, exitCode: exitCode, message: text))
        }
        return text.isEmpty ? .clean : .spoke(notice: text)
    }

    /// Blocking invocation — always called off the main actor.
    nonisolated static func perform(enabled: Bool, name: String) -> Outcome {
        switch TcrTool.resolve() {
        case .failure(let notFound):
            return .failed(
                Failure(
                    enabling: enabled,
                    exitCode: -1,
                    message: "tcr not found (searched \(notFound.searched.count) locations)"
                ))
        case .success(let executable):
            do {
                let output = try TcrTool.run(
                    executable: executable,
                    arguments: arguments(enabled: enabled, name: name)
                )
                return classify(enabling: enabled, exitCode: output.exitCode, stderr: output.stderr)
            } catch {
                return .failed(
                    Failure(enabling: enabled, exitCode: -1, message: error.localizedDescription))
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
    /// The read-back verdict for the last successful toggle of each account —
    /// what the fleet reported afterwards, compared against what was asked.
    @Published public private(set) var verdicts: [String: RecordedVerdict] = [:]

    public init() {}

    public func isPending(_ name: String) -> Bool { pending.contains(name) }
    public func failure(for name: String) -> AccountCommand.Failure? { failures[name] }

    /// Record what the poll that followed a successful toggle reported. Called by
    /// the row immediately after its refresh, so the verdict describes the same
    /// read the row is about to draw.
    /// `notice` is whatever `tcr` wrote to stderr while exiting 0 (see
    /// ``AccountCommand/Outcome/spoke(notice:)``) — normally `nil`. It qualifies
    /// the verdict rather than replacing it.
    public func record(
        readback: PollState,
        requestedEnabled: Bool,
        account name: String,
        notice: String? = nil,
        now: Date = Date()
    ) {
        let verdict = ToggleReadback.verdict(
            requestedEnabled: requestedEnabled,
            account: name,
            readback: readback,
            notice: notice
        )
        verdicts[name] = RecordedVerdict(verdict: verdict, at: now)
        // Every verdict ages out (see ``ToggleReadback/visible(_:reportedDisabled:now:)``),
        // and `visible` is the authority on how long. This timer exists only so the
        // expiry is actually DRAWN: nothing else republishes when the deadline
        // passes, and a row whose account is unchanged will not re-render on its
        // own, so an expired line would linger on screen until something else
        // moved. The identity check keeps a later attempt's verdict safe.
        //
        // It is armed for the unresolved arms too, not just confirmations. Those
        // normally clear on agreement long before their deadline, but the case the
        // deadline exists for — an account that left the fleet, where agreement can
        // never come — is exactly the case where nothing else would ever clear it.
        let recorded = verdicts[name]
        let lifetime = ToggleReadback.lifetime(of: verdict)
        Task { [weak self] in
            try? await Task.sleep(nanoseconds: UInt64(lifetime * 1_000_000_000))
            guard let self, self.verdicts[name] == recorded else { return }
            self.verdicts[name] = nil
        }
    }

    /// The verdict this row may show right now, or `nil`. `reportedDisabled` is
    /// what the *current* fleet read says about the account, which is what stops a
    /// confirmation from outliving its truth.
    public func verdict(
        for name: String,
        reportedDisabled: Bool?,
        now: Date = Date()
    ) -> ToggleVerdict? {
        ToggleReadback.visible(verdicts[name], reportedDisabled: reportedDisabled, now: now)
    }

    /// What a click did, as far as the subprocess can say.
    public enum Attempt: Equatable, Sendable {
        /// A call for this account was already in flight; nothing was run.
        case skipped
        /// A non-zero exit. The words are in ``AccountController/failures``.
        case refused
        /// `tcr` exited 0, so a status refresh is worth doing. `notice` carries
        /// anything it printed to stderr — exit 0 with output is accepted, not
        /// clean, and the caller must pass this to ``record(readback:requestedEnabled:account:notice:now:)``
        /// or the durability half of the outcome dies here.
        case accepted(notice: String?)
    }

    /// Run the toggle. `.accepted` only when `tcr` exited 0 — the caller uses that
    /// to decide whether a status refresh is worth doing, never to update a local
    /// copy of `disabled`.
    @discardableResult
    public func setEnabled(_ enabled: Bool, account name: String) async -> Attempt {
        guard !pending.contains(name) else { return .skipped }
        pending.insert(name)
        // A new attempt clears the previous verdict; a stale error beside a
        // now-succeeding row would be its own lie. The read-back verdict goes for
        // the same reason: a `parked ✓` left over from the last click must not sit
        // beside a call that is still in flight and might not be honoured.
        failures[name] = nil
        verdicts[name] = nil
        defer { pending.remove(name) }

        let outcome = await Task.detached(priority: .userInitiated) {
            AccountCommand.perform(enabled: enabled, name: name)
        }.value

        switch outcome {
        case .clean:
            return .accepted(notice: nil)
        case .spoke(let notice):
            return .accepted(notice: notice)
        case .failed(let failure):
            failures[name] = failure
            return .refused
        }
    }
}
