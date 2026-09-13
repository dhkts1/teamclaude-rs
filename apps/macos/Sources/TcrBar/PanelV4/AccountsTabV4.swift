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
struct AccountsTabV4<Menu: View, Actions: View>: View {
    let fleet: Fleet
    let controlName: String?
    /// Group tokens the operator has opened. Same store the pre-v4 panel used,
    /// so a group left open stays open across this migration.
    let expandedGroups: Set<String>
    let onToggleGroup: (String) -> Void
    let now: Date
    /// The row's own actions, as a context menu — the SECOND route to them,
    /// from the one definition in ``AccountRow``.
    @ViewBuilder var menu: (Account) -> Menu
    /// The same actions as visible controls in the card's trailing slot: the
    /// actions menu, and `Re-login…` on a broken account.
    ///
    /// The mockup draws no per-card gear (`delta-list.md` #27) and the
    /// transcription took that literally, which left `.contextMenu` as the
    /// card's ONLY interaction: seven per-account actions, two of them
    /// destructive, reachable by right-click alone and by no keyboard or
    /// VoiceOver path at all.
    @ViewBuilder var actions: (Account) -> Actions

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

    private func card(_ row: FleetSectionRow, shape: AccountCard<Actions>.Shape) -> some View {
        AccountCard(account: row.account, shape: shape, now: now, isControl: row.isControl) {
            actions(row.account)
        }
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
                // The whole of a collapsed live group: one line and its tally.
                // Every string here is a ``FleetSection`` property, so what is
                // drawn cannot drift from what the group holds.
                V4Row {
                    DimText(text: section.collapsedSummaryLine)
                } trailing: {
                    HStack(spacing: V4.pillGap) {
                        ForEach(section.breakdown, id: \.kind.token) { tally in
                            V4Pill(text: tally.label, role: pillRole(tally.kind))
                        }
                        // The control account can be a member of a group
                        // small enough to collapse. Its own card is gone —
                        // the whole point of collapsing — so the one line
                        // left is where the fact has to live, after the
                        // tallies it did not change.
                        if section.containsControl {
                            V4Pill(
                                text: "Control",
                                help: "The control account for this fleet is inside this group.")
                        }
                    }
                }
            } else {
                // `.grp .card{margin:6px 0}` — EVERY card, the first included.
                // Its top margin does not collapse into the group's own 12 pt
                // padding, so the first card sits 18 pt under the stroke; giving
                // the first card nothing put it at 11 pt. The LAST card carries
                // the same margin on its OWN bottom, but only when nothing else
                // follows it: `GroupBox`'s own bottom padding (4 pt) plus this
                // 6 pt closes the box at the mockup's 10, and a disclosure row
                // right below the cards gets the 6 pt instead, so the two never
                // stack into 12.
                ForEach(Array(visible.enumerated()), id: \.element.id) { index, row in
                    card(row, shape: .compact)
                        .padding(.top, V4.groupCardGap)
                        .padding(
                            .bottom,
                            index == visible.count - 1 && !isCollapsible(section)
                                ? V4.groupCardGap : 0)
                }
            }
            if isCollapsible(section) {
                V4Disclosure(
                    title: disclosureTitle(section, expanded: expanded, hidden: hidden),
                    expanded: expanded,
                    help: expanded
                        ? "Collapses this group again."
                        : "Shows every account in this group."
                ) { onToggleGroup(section.group.token) }
                .padding(.bottom, V4.groupCardGap)
            }
        }
        // A group that leads the tab collapses its own 20 pt top margin with the
        // strip's 12 the same way a card does. ``GroupBox`` has already applied
        // the full margin, so the collapse is the difference — negative, and it
        // is a subtraction rather than a parameter because the margin belongs to
        // the group, not to whoever happens to draw it first.
        .padding(.top, first ? V4.marginAfterStrip(V4.groupMarginTop) - V4.groupMarginTop : 0)
    }

    /// Whether this group has a disclosure at all — whether it has ever hidden
    /// anything, in either direction.
    ///
    /// Both call sites used to sit behind `if summarised` / `if hidden > 0`, so
    /// once a group was open BOTH were false and no control rendered. Expanding
    /// was a one-way door, and the state persists to `UserDefaults` across
    /// launches: the thing that vanished was the control the user had just
    /// pressed, which also orphans focus under assistive tech. The pre-v4 panel
    /// kept the reverse path on the legend button with a "Collapses this group."
    /// hint.
    ///
    /// A wholly-parked group of three or fewer is NOT collapsible: it caps at
    /// ``parkedVisibleRows`` and so has nothing to hide, and a control that does
    /// nothing is worse than none.
    private func isCollapsible(_ section: FleetSection) -> Bool {
        section.collapsesByDefault
            || (section.isWhollyParked && section.rows.count > Self.parkedVisibleRows)
    }

    /// One direction each. ``FleetSection/expandButtonLabel`` says the whole
    /// count ("Show 6 accounts in this group") because a collapsed group shows
    /// no cards at all; a capped parked group says the REMAINDER, because some
    /// are already on screen. Closing says neither number — what it removes is
    /// whatever is currently open.
    private func disclosureTitle(
        _ section: FleetSection, expanded: Bool, hidden: Int
    ) -> String {
        if expanded { return "Show fewer accounts" }
        if isSummarised(section) { return section.expandButtonLabel }
        return "Show \(hidden) more \(hidden == 1 ? "account" : "accounts")"
    }

    private func pillRole(_ kind: FleetTally.Kind) -> V4Pill.Role {
        switch kind {
        case .ok: return .ok
        case .near: return .warn
        case .spent, .needsRelogin, .rejected: return .bad
        case .unmeasured: return .info
        case .unknown, .disabled: return .neutral
        }
    }
}
