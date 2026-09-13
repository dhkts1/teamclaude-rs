import SwiftUI
import TcrBarCore

/// The Accounts tab, in the mockup's own DOM order: the loose cards first, then
/// one `.grp` box per named group, then the tab's action row.
///
/// The order is the one delta the sheet and the shipping panel disagreed about:
/// the mockup runs loose cards, then the parked group, then the collapsed one,
/// while the panel drew its groups first and the ungrouped pile between them. Loose cards lead here — they are the accounts
/// that answer "what can serve right now" with no label to read first.
///
/// Then the groups, in ``Fleet/sectionsInDisplayOrder(pinning:)``'s order
/// (unchanged, still unit-tested there) but with the ones that are drawn as a
/// single summary line LAST. That is the mockup's own order —
/// `HENRY-TOKEN · PARKED` with its member cards, then the collapsed
/// `MYCELIUM · ACTIVE` — and it is a rule, not a transcription: a box holding
/// one line is a lid, and a lid between two open boxes reads as the end of the
/// list.
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
        let loose = all.filter { $0.group == .ungrouped }
        let named = all.filter { $0.group != .ungrouped }
        return loose + named.filter { !isSummarised($0) } + named.filter { isSummarised($0) }
    }

    /// A group the operator has not opened and that the fleet says collapses by
    /// default: one line, its tally, and a button.
    private func isSummarised(_ section: FleetSection) -> Bool {
        section.collapsesByDefault && !expandedGroups.contains(section.group.token)
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            ForEach(Array(sections.enumerated()), id: \.element.id) { index, section in
                if section.group == .ungrouped {
                    ForEach(Array(section.rows.enumerated()), id: \.element.id) { row, account in
                        card(account, shape: .full)
                            .padding(
                                .top,
                                index == 0 && row == 0
                                    ? V4.marginAfterStrip(V4.cardGap) : V4.cardGap)
                    }
                } else {
                    group(section, first: index == 0)
                }
            }
        }
    }

    private func card(_ row: FleetSectionRow, shape: AccountCard.Shape) -> some View {
        AccountCard(account: row.account, shape: shape, now: now)
            .contextMenu { menu(row.account) }
    }

    @ViewBuilder
    private func group(_ section: FleetSection, first: Bool) -> some View {
        let expanded = expandedGroups.contains(section.group.token)
        let summarised = isSummarised(section)
        let capped = section.isWhollyParked && !expanded
        let visible =
            capped ? Array(section.rows.prefix(Self.parkedVisibleRows)) : Array(section.rows)
        let hidden = section.rows.count - visible.count

        GroupBox(
            legend: section.legendText,
            color: section.outlineColor.map(V4.groupColor),
            collapsed: summarised
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
                // `.grp .card{margin:6px 0}` — EVERY card, the first included.
                // Its top margin does not collapse into the group's own 12 pt
                // padding, so the first card sits 18 pt under the stroke; giving
                // the first card nothing put it at 11 pt.
                ForEach(visible) { row in
                    card(row, shape: .compact)
                        .padding(.top, V4.groupCardGap)
                }
                if hidden > 0 {
                    V4Disclosure(
                        title: "Show \(hidden) more \(hidden == 1 ? "account" : "accounts")",
                        help: "Shows the rest of this parked group."
                    ) { onToggleGroup(section.group.token) }
                }
            }
        }
        // A group that leads the tab collapses its own 20 pt top margin with the
        // strip's 12 the same way a card does. ``GroupBox`` has already applied
        // the full margin, so the collapse is the difference — negative, and it
        // is a subtraction rather than a parameter because the margin belongs to
        // the group, not to whoever happens to draw it first.
        .padding(.top, first ? V4.marginAfterStrip(V4.groupMarginTop) - V4.groupMarginTop : 0)
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
