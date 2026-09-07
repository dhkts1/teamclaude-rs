import Foundation

/// How this app names ONE account, everywhere: the display name plus the org it
/// belongs to.
///
/// It exists because a name is not an identity. The fleet holds the same email
/// twice — once in a personal Max org, once in the company Team org — and until
/// this type existed the panel had no way to tell those two rows apart. The
/// consequences were not subtle:
///
///  * `ForEach(…, id: \.element.id)` collapsed the pair into ONE SwiftUI
///    identity, so both rows drew the first row's quota bars and neither wore
///    its own gate pill, while `tcr status --json` reported the two correctly
///    and differently. A screenshot of the panel disagreed with the CLI.
///  * Every per-account dictionary — in-flight toggles, failures, read-back
///    verdicts, removed-needs-restart — was keyed by name, so a failure on one
///    row rendered on both.
///  * Every command the panel issued carried a bare name, and `tcr` refuses an
///    ambiguous one rather than guessing: "Copy Access Token" on either row
///    failed with `'…' is ambiguous — matches 2 accounts … Narrow with --org`.
///
/// The two halves are deliberately kept separate rather than pre-joined into a
/// string, because they are used for different things and must not be confused:
/// ``id`` is what the UI keys on, and ``name``/``orgUuid`` are what the CLI is
/// handed (`tcr token <name> --org <uuid>`). `tcr` has never taken the joined
/// form and must never be passed it.
public struct AccountRef: Hashable, Sendable {
    /// The account's display name — an email on this fleet. What `tcr` matches
    /// positionally.
    public let name: String
    /// The org UUID, when the server reported one. Passed as `--org` to narrow
    /// an ambiguous `name`; `nil` from a server built before the org keys
    /// existed, in which case no flag is passed and behaviour is exactly what it
    /// was.
    public let orgUuid: String?

    public init(name: String, orgUuid: String? = nil) {
        self.name = name
        // An empty string is not an org. Normalizing here rather than at each
        // call site is what keeps `--org ""` — which `tcr` would match nothing
        // for — from ever being built.
        self.orgUuid = (orgUuid?.isEmpty == false) ? orgUuid : nil
    }

    /// The single definition of this app's per-account identity. `Account.id`
    /// and every by-account dictionary key resolve to this, so a row's SwiftUI
    /// identity and its verdict's dictionary key cannot disagree.
    ///
    /// Falls back to the bare `name` when there is no org, which keeps an older
    /// server's rows rendering exactly as they do today — and is the honest
    /// answer there: with no org on the wire, the name is all the identity this
    /// build can see.
    ///
    /// The `|` separator cannot collide with a real value: an org UUID contains
    /// no `|`, so `a|uuid` is reachable only from `AccountRef(name: "a",
    /// orgUuid: "uuid")`.
    public var id: String {
        guard let orgUuid else { return name }
        return "\(name)|\(orgUuid)"
    }
}

extension Account {
    /// This row, as the identity every controller and command should take.
    public var ref: AccountRef { AccountRef(name: name, orgUuid: orgUuid) }
}
