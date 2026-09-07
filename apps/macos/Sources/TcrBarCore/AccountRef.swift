import Foundation

/// How this app names ONE account, everywhere: its name, which is unique across
/// the fleet.
///
/// It carried an org UUID alongside the name until account names became unique.
/// The fleet held the same email twice — once in a personal Max org, once in the
/// company Team org — and the panel had no way to tell those two rows apart. The
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
///    failed with `'…' is ambiguous — matches 2 accounts`.
///
/// `tcr` fixed that below this app: two rows can no longer share a name, so the
/// name IS the identity and there is nothing left to pair it with. The type
/// survives the collapse because it is the one place that statement lives — a
/// row's SwiftUI identity, its dictionary key and the argument handed to `tcr`
/// all resolve here, so they cannot drift apart again.
public struct AccountRef: Hashable, Sendable {
    /// The account's name — an email, or `email/<org-slug>` on a fleet where one
    /// person holds two orgs. Unique, and what `tcr` matches exactly.
    public let name: String

    public init(name: String) {
        self.name = name
    }

    /// The single definition of this app's per-account identity. `Account.id`
    /// and every by-account dictionary key resolve to this, so a row's SwiftUI
    /// identity and its verdict's dictionary key cannot disagree.
    public var id: String { name }
}

extension Account {
    /// This row, as the identity every controller and command should take.
    public var ref: AccountRef { AccountRef(name: name) }
}
