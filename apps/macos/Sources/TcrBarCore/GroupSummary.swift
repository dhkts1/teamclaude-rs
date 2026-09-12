import Foundation

/// One row's worth of a group, derived from the fleet's own accounts rather
/// than a dedicated wire object — there isn't one; `groups`/`reservedGroups`/
/// `parkedGroups`/`groupColors` are all per-account fields
/// (`FleetStatus.swift`), repeated on every member.
///
/// Exists so the Settings window's Groups & Rotation pane can list **every**
/// group the fleet actually has, not just the ones with an account row
/// visible on the panel right now. The mockup review's S8 finding was exactly
/// this failure a layer up: two groups appeared only as bare names with no
/// settings drawn, because the pane that reviewed them iterated the panel's
/// own (collapsed) card list instead of the fleet's real membership.
public struct GroupSummary: Equatable, Sendable, Identifiable {
    public var id: String { name }
    public let name: String
    /// How many accounts carry this label, wherever they sit on the panel.
    public let memberCount: Int
    /// True when every member with a wire opinion has parked it — mirrors
    /// `groupSettings.<g>.parked` observed indirectly through the accounts
    /// that carry `parkedGroups`.
    public let isParked: Bool
    public let isReserved: Bool
    /// `nil` when no member carries a colour for this group — the neutral
    /// fallback every other group-colour reader in this app already uses.
    public let colorHex: String?

    public init(
        name: String, memberCount: Int, isParked: Bool, isReserved: Bool, colorHex: String?
    ) {
        self.name = name
        self.memberCount = memberCount
        self.isParked = isParked
        self.isReserved = isReserved
        self.colorHex = colorHex
    }

    /// Every group name any account in `accounts` carries, each summarised
    /// once. Sorted by name so the pane's row order is stable across polls —
    /// a settings list that reorders itself between two reads of the same
    /// fleet is a worse defect than an alphabetical one that never puts the
    /// group you are looking for first.
    public static func summarize(_ accounts: [Account]) -> [GroupSummary] {
        var names: Set<String> = []
        for account in accounts {
            names.formUnion(account.groups ?? [])
        }
        return names.sorted().map { name in
            let members = accounts.filter { ($0.groups ?? []).contains(name) }
            let parkedMembers = members.filter { ($0.parkedGroups ?? []).contains(name) }
            let reservedMembers = members.filter { ($0.reservedGroups ?? []).contains(name) }
            let color = members.compactMap { $0.groupColors?[name] }.first
            return GroupSummary(
                name: name,
                memberCount: members.count,
                // Parked/reserved are group-wide settings server-side
                // (`groupSettings.<g>`), so any one member's wire copy speaks
                // for the whole group; "any" rather than "all" is the safer
                // read when members disagree, since that disagreement is
                // itself something a boot-time snapshot can produce and the
                // pane should not paper over by picking `all`.
                isParked: !parkedMembers.isEmpty,
                isReserved: !reservedMembers.isEmpty,
                colorHex: color
            )
        }
    }
}
