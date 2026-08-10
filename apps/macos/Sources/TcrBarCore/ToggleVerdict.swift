import Foundation

/// What happened to a toggle, established by RE-READING the fleet rather than by
/// assuming.
///
/// The defect this exists to close, measured on the live fleet: `tcr disable
/// <name>` rewrites `~/.config/teamclaude.json` and exits 0, but a running proxy
/// read `disabled` into memory once at boot and `tcr status` prefers the live
/// server — so the row re-polled, got the OLD value back, and changed nothing.
/// Exit 0 meant ``AccountController/failures`` stayed empty, so the row said
/// nothing at all while the account kept taking traffic. A success that is
/// indistinguishable from doing nothing is the worst of the three outcomes to
/// render, because it is the one nobody investigates.
///
/// So a toggle has three renderable outcomes and this type carries the two that
/// only a re-read can tell apart. The third — a non-zero exit — is
/// ``AccountCommand/Failure`` and is unchanged: `tcr`'s own words, verbatim.
///
/// A fourth arm, ``spokeUp(notice:about:)``, closes the sibling of that defect at
/// the *process* boundary rather than the poll boundary. `tcr` can exit 0 and
/// still tell you the change is only half done — the park applied to the live
/// rotation but no config entry matched it, so it comes back on restart
/// (`src/cli.rs`, `SetDisabledOutcome`'s `warning`), or the running proxy was too
/// old for the control route and only the file changed. Both are printed to
/// stderr with exit 0. The read-back cannot see either: `disabled` DID flip, so
/// the comparison below confirms, and the row used to stamp `parked ✓` on a
/// change that will not survive a restart. The rule is structural, not lexical —
/// **a zero exit with anything on stderr is not a clean success** — so a warning
/// nobody has written yet lands on this arm too.
///
/// `requestedEnabled` is what the click ASKED for (`true` = put back in
/// rotation), never what the row happened to show.
public enum ToggleVerdict: Equatable, Sendable {
    /// The fleet now reports the state that was asked for.
    case confirmed(requestedEnabled: Bool)
    /// `tcr` exited 0 and the fleet still reports the old state. This is the arm
    /// that fires on a machine whose proxy predates the fix, and on a machine
    /// whose proxy simply has not restarted yet — both deploy orders land here,
    /// which is why it names the mechanism instead of just failing.
    case notHonoured(requestedEnabled: Bool)
    /// `tcr` exited 0 but the re-read could not answer: the status call itself
    /// failed, the payload did not decode, or the account is no longer listed.
    /// Distinct from both of the above on purpose — "I could not check" is not a
    /// confirmation, and it is not evidence the proxy disagreed either.
    case unverified(requestedEnabled: Bool, reason: String)
    /// `tcr` exited 0 **and wrote to stderr**, so whatever the re-read then said
    /// is qualified by words this app is not allowed to interpret. It wraps the
    /// read-back verdict rather than replacing it, because the two facts are
    /// independent: the live rotation may well have changed (`about:
    /// .confirmed`) while the change is not durable, and an old proxy's
    /// "only the config file was changed" arrives beside a `.notHonoured` whose
    /// remedy is in the notice and nowhere else.
    ///
    /// `notice` is `tcr`'s bytes, trimmed of surrounding whitespace and
    /// otherwise verbatim. Nothing here greps it: a keyword test would pass every
    /// warning added after the test was written, which is the exact failure this
    /// arm exists to prevent. It never nests — the only construction site is
    /// ``ToggleReadback/verdict(requestedEnabled:account:readback:notice:)``,
    /// which builds it from a plain read-back verdict.
    indirect case spokeUp(notice: String, about: ToggleVerdict)

    public var requestedEnabled: Bool {
        switch self {
        case .confirmed(let enabled), .notHonoured(let enabled): return enabled
        case .unverified(let enabled, _): return enabled
        case .spokeUp(_, let about): return about.requestedEnabled
        }
    }

    /// True only for the arm that asserts the change took effect, cleanly.
    /// Anything that branches on "did this work" must read this and not `case
    /// let`, so a future arm cannot default into looking like a success.
    ///
    /// ``spokeUp(notice:about:)`` is deliberately NOT a confirmation even when it
    /// wraps one: `tcr` said something on the success path, and the whole point of
    /// the arm is that "the flag flipped" and "the change will survive a restart"
    /// are different claims.
    public var isConfirmation: Bool {
        if case .confirmed = self { return true }
        return false
    }

    /// The row's own vocabulary: `rotating` / `parked`, matching the pill, so the
    /// verdict line and the pill can never use two different words for one state.
    private static func word(rotating: Bool) -> String { rotating ? "rotating" : "parked" }

    /// One line for the row. Always non-empty.
    ///
    /// The `✓` belongs to ``confirmed(requestedEnabled:)`` alone. A qualified
    /// confirmation spells the same fact out in words instead, so a glance can
    /// never read it as the clean case — the tick is the thing an operator scans
    /// for, and putting one on a change that may not survive a restart is the
    /// defect this arm was added for.
    public var rowLabel: String {
        switch self {
        case .confirmed(let enabled):
            return "\(Self.word(rotating: enabled)) ✓"
        case .notHonoured, .unverified:
            return statement
        case .spokeUp(let notice, let about):
            return "\(about.statement) — tcr said: \(notice)"
        }
    }

    /// The same outcome with no `✓` in it, for composing under
    /// ``spokeUp(notice:about:)``.
    private var statement: String {
        switch self {
        case .confirmed(let enabled):
            return "the fleet now reports \(Self.word(rotating: enabled))"
        case .notHonoured(let enabled):
            // The old state is the opposite of what was asked for — that is what
            // makes this arm this arm.
            return "tcr accepted it — the running proxy still reports \(Self.word(rotating: !enabled))"
        case .unverified(_, let reason):
            return "tcr accepted it — could not confirm: \(reason)"
        case .spokeUp(let notice, let about):
            // Never constructed; see the arm's own documentation. Rendering the
            // notice rather than dropping it keeps a construction bug legible.
            return "\(about.statement) — tcr said: \(notice)"
        }
    }

    /// The same fact spoken. A confirmation only a sighted user gets is half
    /// built, and ``rowLabel``'s `✓` is punctuation to VoiceOver.
    public var spokenLabel: String {
        switch self {
        case .confirmed(let enabled):
            return enabled ? "confirmed rotating" : "confirmed parked, out of rotation"
        case .notHonoured(let enabled):
            return "tcr accepted the change but the running proxy still reports "
                + Self.word(rotating: !enabled)
        case .unverified(_, let reason):
            return "tcr accepted the change but it could not be confirmed: \(reason)"
        case .spokeUp(let notice, let about):
            // "confirmed" is withheld here for the same reason the `✓` is: the
            // spoken form has to be as qualified as the written one.
            return "\(about.spokenStatement), and tcr said: \(notice)"
        }
    }

    /// ``spokenLabel`` with the unqualified "confirmed" removed, for composing
    /// under ``spokeUp(notice:about:)``.
    private var spokenStatement: String {
        switch self {
        case .confirmed(let enabled):
            return enabled
                ? "the fleet now reports rotating" : "the fleet now reports parked, out of rotation"
        case .notHonoured, .unverified, .spokeUp:
            return spokenLabel
        }
    }
}

/// A verdict plus when it was reached. The timestamp is what keeps a
/// confirmation from outliving its truth.
public struct RecordedVerdict: Equatable, Sendable {
    public let verdict: ToggleVerdict
    public let at: Date

    public init(verdict: ToggleVerdict, at: Date) {
        self.verdict = verdict
        self.at = at
    }
}

/// The pure half of the toggle read-back: turn a requested state plus whatever
/// the next poll reported into a verdict, and decide whether a stored verdict is
/// still true enough to show.
///
/// Pure on purpose. `ImageRenderer` cannot draw AppKit controls at all — every
/// `--render-states` PNG shows a placeholder where this row's button is — so no
/// snapshot can ever cover this logic. If it is not testable without a view it
/// is not tested.
public enum ToggleReadback {
    /// How long a confirmation may sit on screen: two poll intervals
    /// (``StatusPoller`` polls every 3s), so a confirmation always survives at
    /// least one full refresh and then goes away on its own.
    public static let confirmationLifetime: TimeInterval = 6

    /// How long a confirmation that carries a `tcr` notice may sit on screen.
    /// Longer than ``confirmationLifetime`` because it is not an acknowledgement
    /// the operator can ignore: it is the only place the warning appears — `tcr`
    /// wrote it to a stderr no one is reading — and it names something still to
    /// be done. It does age out, because nothing can re-verify it and the panel is
    /// a live view rather than a log.
    public static let noticeLifetime: TimeInterval = 60

    /// How long a ``ToggleVerdict/notHonoured(requestedEnabled:)`` or
    /// ``ToggleVerdict/unverified(requestedEnabled:reason:)`` may sit on screen
    /// when the fleet has not resolved it.
    ///
    /// These arms clear on agreement, so an expiry looks redundant — and it is,
    /// as long as the account stays in the fleet. It does not always: with the
    /// account gone, `reportedDisabled` is `nil`, `nil == !requested` is never
    /// true, and the verdict was retained *forever*. An account that left the
    /// fleet and returned an hour later dragged a stale line back with it. So the
    /// rule is an **expiry, not a clear**: fifteen minutes from the moment the
    /// verdict was reached, whether or not the account is still listed. Long
    /// enough that a real disagreement is never hidden from the operator who
    /// caused it (the original defect was a row that said *nothing*), short
    /// enough that it cannot reappear beside a later state it knows nothing about.
    public static let unresolvedLifetime: TimeInterval = 15 * 60

    /// How long this verdict may be rendered after it was reached. Total over the
    /// arms on purpose: an arm with no lifetime is an arm that can outlive its
    /// truth, and that is how the absence bug above got in.
    public static func lifetime(of verdict: ToggleVerdict) -> TimeInterval {
        switch verdict {
        case .confirmed: return confirmationLifetime
        case .notHonoured, .unverified: return unresolvedLifetime
        case .spokeUp(_, let about):
            if case .confirmed = about { return noticeLifetime }
            return unresolvedLifetime
        }
    }

    /// The comparison. `requestedEnabled` is what was asked; `readback` is the
    /// poll that followed the successful call.
    ///
    /// `notice` is whatever `tcr` wrote to stderr while exiting 0 — normally
    /// nothing. When it is non-empty the verdict is wrapped in
    /// ``ToggleVerdict/spokeUp(notice:about:)`` whatever the re-read said, because
    /// exit 0 plus output is not a clean success and this app does not read `tcr`'s
    /// prose to decide how bad it is.
    public static func verdict(
        requestedEnabled: Bool,
        account name: String,
        readback: PollState,
        notice: String? = nil
    ) -> ToggleVerdict {
        let plain = plainVerdict(
            requestedEnabled: requestedEnabled, account: name, readback: readback)
        guard let notice, !notice.isEmpty else { return plain }
        return .spokeUp(notice: notice, about: plain)
    }

    /// The read-back comparison alone, with no knowledge of what `tcr` printed.
    private static func plainVerdict(
        requestedEnabled: Bool,
        account name: String,
        readback: PollState
    ) -> ToggleVerdict {
        switch readback {
        case .loaded(let fleet):
            guard let account = fleet.accounts.first(where: { $0.name == name }) else {
                return .unverified(
                    requestedEnabled: requestedEnabled,
                    reason: "the fleet no longer lists this account"
                )
            }
            // `disabled` is the field the config and the live server disagree
            // about, so it is the only thing worth comparing. `status` keeps
            // saying "active" on a disabled account (verified against live
            // output) and would confirm every toggle.
            return account.disabled == !requestedEnabled
                ? .confirmed(requestedEnabled: requestedEnabled)
                : .notHonoured(requestedEnabled: requestedEnabled)
        case .pending:
            return .unverified(requestedEnabled: requestedEnabled, reason: "no fleet read has completed")
        case .toolMissing, .commandFailed, .undecodable:
            return .unverified(requestedEnabled: requestedEnabled, reason: readback.summary)
        }
    }

    /// Whether a stored verdict may still be rendered, given what the fleet
    /// reports for that account *now*. `reportedDisabled` is `nil` when the
    /// current poll has no row for it.
    ///
    /// Two different lifetimes, because the two arms make different claims:
    ///
    ///  - A **confirmation** asserts "the fleet reports what you asked for". It
    ///    drops the moment that stops being true — a rotation change, or an
    ///    account that vanished — and otherwise ages out after
    ///    ``confirmationLifetime``. A `parked ✓` still sitting beside a row that
    ///    has since gone back into rotation is exactly the class of lie this
    ///    whole type exists to prevent, and an ack that never expires becomes
    ///    one eventually by sitting there.
    ///  - A **not-honoured** verdict names an UNRESOLVED disagreement between
    ///    the config and the running proxy. It deliberately does not age out;
    ///    hiding it after six seconds would restore the original defect, where
    ///    the row said nothing. It clears only when the fleet finally reports the
    ///    requested state — i.e. when the proxy actually caught up.
    ///  - **unverified** clears on the same condition, and never promotes itself
    ///    to a confirmation: nothing observed the change take effect, so this
    ///    only ever stops being said, it is never upgraded.
    ///  - A **notice** (``ToggleVerdict/spokeUp(notice:about:)``) follows the
    ///    clearing rule of the verdict it wraps — it is a qualification of that
    ///    verdict, not a separate claim — and gets its own, longer lifetime.
    ///
    /// Every arm now has a lifetime (``lifetime(of:)``), including the unresolved
    /// ones. That is the fix for a verdict about a departed account: agreement can
    /// never clear it, because `nil` is not `!requested`, so an expiry has to.
    public static func visible(
        _ recorded: RecordedVerdict?,
        reportedDisabled: Bool?,
        now: Date
    ) -> ToggleVerdict? {
        guard let recorded else { return nil }
        guard now.timeIntervalSince(recorded.at) < lifetime(of: recorded.verdict) else { return nil }
        let requested = recorded.verdict.requestedEnabled
        let fleetAgrees = reportedDisabled == !requested
        switch recorded.verdict {
        case .confirmed, .spokeUp(_, .confirmed):
            return fleetAgrees ? recorded.verdict : nil
        case .notHonoured, .unverified, .spokeUp:
            return fleetAgrees ? nil : recorded.verdict
        }
    }
}
