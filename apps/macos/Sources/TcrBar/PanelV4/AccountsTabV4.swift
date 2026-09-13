import SwiftUI
import TcrBarCore

/// The Accounts tab, in the mockup's own DOM order: the loose cards first, then
/// one `.grp` box per named group, then the tab's action row.
///
/// The order is the one delta the sheet and the shipping panel disagreed about
/// (`/tmp/parity/delta-list.md` #26): the panel drew its groups first and the
/// ungrouped pile between them. Loose cards lead here — they are the accounts
/// that answer "what can serve right now" with no label to read first — and the
/// named groups follow in ``Fleet/sectionsInDisplayOrder(pinning:)``'s order,
/// which is unchanged and still unit-tested there.
struct AccountsTabV4<Menu: View>: View {
    let fleet: Fleet
    let controlName: String?
    /// Group tokens the operator has opened. Same store the pre-v4 panel used,
    /// so a group left open stays open across this migration.
    let expandedGroups: Set<String>
    let onToggleGroup: (String) -> Void
    let now: Date
    /// The row's own actions, as a context menu. The mockup draws no per-card
    /// gear (`delta-list.md` #27), so the card carries no visible control — but
    /// every action the pre-v4 row offered is still on the card itself, by
    /// right-click, from the one definition in ``AccountRow``.
    @ViewBuilder var menu: (Account) -> Menu

    /// How many accounts a parked group shows before its "Show N more accounts"
    /// button — the mockup's HENRY-TOKEN group draws three of its five.
    static var parkedVisibleRows: Int { 3 }

    private var sections: [FleetSection] {
        let all = fleet.sectionsInDisplayOrder(pinning: controlName)
        return all.filter { $0.group == .ungrouped } + all.filter { $0.group != .ungrouped }
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            ForEach(sections, id: \.id) { section in
                if section.group == .ungrouped {
                    ForEach(section.rows) { row in
                        card(row, shape: .full)
                            .padding(.top, V4.cardGap)
                    }
                } else {
                    group(section)
                }
            }
        }
    }

    private func card(_ row: FleetSectionRow, shape: AccountCard.Shape) -> some View {
        AccountCard(account: row.account, shape: shape, now: now)
            .contextMenu { menu(row.account) }
    }

    @ViewBuilder
    private func group(_ section: FleetSection) -> some View {
        let expanded = expandedGroups.contains(section.group.token)
        let summarised = section.collapsesByDefault && !expanded
        let capped = section.isWhollyParked && !expanded
        let visible =
            capped ? Array(section.rows.prefix(Self.parkedVisibleRows)) : Array(section.rows)
        let hidden = section.rows.count - visible.count

        GroupBox(
            legend: section.legendText,
            color: section.outlineColor.map(V4.groupColor) ?? Tok.cardLine
        ) {
            if summarised {
                // The whole of a collapsed live group: one line, its tally, and
                // the control that opens it. Every string here is a
                // ``FleetSection`` property, so what is drawn cannot drift from
                // what the group holds.
                V4Row {
                    DimText(text: section.collapsedSummaryLine)
                } trailing: {
                    HStack(spacing: V4.pillGap) {
                        ForEach(section.breakdown, id: \.kind.token) { tally in
                            V4Pill(text: tally.label, role: pillRole(tally.kind))
                        }
                    }
                }
                V4Disclosure(
                    title: section.expandButtonLabel,
                    help: "Shows every account in this group."
                ) { onToggleGroup(section.group.token) }
            } else {
                ForEach(Array(visible.enumerated()), id: \.element.id) { index, row in
                    card(row, shape: .compact)
                        .padding(.top, index == 0 ? 0 : V4.groupCardGap)
                }
                if hidden > 0 {
                    V4Disclosure(
                        title: "Show \(hidden) more \(hidden == 1 ? "account" : "accounts")",
                        help: "Shows the rest of this parked group."
                    ) { onToggleGroup(section.group.token) }
                }
            }
        }
    }

    private func pillRole(_ kind: FleetTally.Kind) -> V4Pill.Role {
        switch kind {
        case .ok: return .ok
        case .near: return .warn
        case .spent, .needsRelogin: return .bad
        case .unmeasured: return .info
        case .unknown, .disabled: return .neutral
        }
    }
}
