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

    public var requestedEnabled: Bool {
        switch self {
        case .confirmed(let enabled), .notHonoured(let enabled): return enabled
        case .unverified(let enabled, _): return enabled
        }
    }

    /// True only for the arm that asserts the change took effect. Anything that
    /// branches on "did this work" must read this and not `case let`, so a future
    /// fourth arm cannot default into looking like a success.
    public var isConfirmation: Bool {
        if case .confirmed = self { return true }
        return false
    }

    /// The row's own vocabulary: `rotating` / `parked`, matching the pill, so the
    /// verdict line and the pill can never use two different words for one state.
    private static func word(rotating: Bool) -> String { rotating ? "rotating" : "parked" }

    /// One line for the row. Always non-empty.
    public var rowLabel: String {
        switch self {
        case .confirmed(let enabled):
            return "\(Self.word(rotating: enabled)) ✓"
        case .notHonoured(let enabled):
            // The old state is the opposite of what was asked for — that is what
            // makes this arm this arm.
            return "tcr accepted it — the running proxy still reports \(Self.word(rotating: !enabled))"
        case .unverified(_, let reason):
            return "tcr accepted it — could not confirm: \(reason)"
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

    /// The comparison. `requestedEnabled` is what was asked; `readback` is the
    /// poll that followed the successful call.
    public static func verdict(
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
    public static func visible(
        _ recorded: RecordedVerdict?,
        reportedDisabled: Bool?,
        now: Date,
        lifetime: TimeInterval = confirmationLifetime
    ) -> ToggleVerdict? {
        guard let recorded else { return nil }
        let requested = recorded.verdict.requestedEnabled
        let fleetAgrees = reportedDisabled == !requested
        switch recorded.verdict {
        case .confirmed:
            guard fleetAgrees else { return nil }
            guard now.timeIntervalSince(recorded.at) < lifetime else { return nil }
            return recorded.verdict
        case .notHonoured, .unverified:
            return fleetAgrees ? nil : recorded.verdict
        }
    }
}
