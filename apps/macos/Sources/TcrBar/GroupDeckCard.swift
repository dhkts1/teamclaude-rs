import SwiftUI
import TcrBarCore

/// One group-SET's card in the sectioned accounts list — collapses a
/// section down to ONE card with its members "stacked" behind it like a
/// deck, instead of a header followed by every member's full row (bridge:
/// `docs/plans/stacked-cards-bridge.md`). Hover peeks the deck open within a
/// fixed footprint, a click pins it open to today's real ``AccountRow``
/// list, and hovering a row inside the open list lifts it while its
/// neighbours recede.
///
/// ## The `RowHeightsKey` sizing hazard — read before changing this file
///
/// `FleetView` sizes the whole scroll viewport from summed, MEASURED row
/// heights (`RowHeightsKey`, `FleetView.visibleRowsHeight(for:)`), so the
/// popover grows with the fleet. A card whose footprint changed on **hover**
/// would move that sum — and the popover's height with it — while the
/// pointer sat inside it, which reads as the panel jumping under the
/// cursor. The bridge's own words: "a panel that jumps under the pointer is
/// a worse outcome than a slightly less magical one."
///
/// This view resolves that by never letting hover touch layout at all.
/// `isHovering`-gated code below (``collapsedContent``, ``peekIndicator``)
/// only ever applies `scaleEffect`/`shadow`/`opacity` — it never toggles
/// `isOpen`, never adds or removes a view, and never registers a different
/// value with `RowHeightsKey`: the collapsed body always renders exactly
/// ``topMember``'s row plus a constant decorative peek strip, at exactly
/// the same size, whether or not the pointer is over it. Only ``isOpen`` —
/// set by a click, never by `onHover` — swaps that fixed one-row body for
/// the real per-member list, which DOES resize the panel. That is a
/// deliberate, discrete action, not a value drifting while the pointer
/// merely rests over the card, so it does not carry the hazard's defect:
/// this is the "ship click-to-expand with a non-resizing hover peek" branch
/// the bridge asked for when hover-driven expansion cannot be made stable,
/// and hover-driven expansion was never attempted here for exactly that
/// reason — not attempted and found unstable, judged unstable by
/// construction (a `ScrollView` sized from summed child heights has no
/// stable way to grow toward the pointer without moving content under it)
/// and designed around from the start.
///
/// Verifiable by inspection rather than a screenshot harness: grep this
/// file for `isOpen =` — the only assignment is in ``toggleOpen()``, reached
/// only from the header's `Button` action, never from an `onHover` closure.
struct GroupDeckCard: View {
    let section: GroupSection
    /// The whole fleet, threaded down to ``GroupActionsMenu`` and each
    /// member row's own "Add to group…" menu, same reason
    /// `AccountGroupSectionHeader` already needed it.
    let allAccounts: [Account]
    let countersAreStructural: Bool
    @ObservedObject var accounts: AccountController
    @ObservedObject var control: ControlAccountController
    @ObservedObject var groupController: GroupController
    let onChanged: () async -> PollState
    let onRelogin: (String) -> Void
    @Binding var confirmRemoveGroup: String?

    @State private var isOpen = false
    @State private var isHovering = false
    @State private var hoveredMemberID: String?
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    private var isUngrouped: Bool { section.groups.isEmpty }
    private var topMember: Account? { section.members.first }
    private var restMembers: ArraySlice<Account> { section.members.dropFirst() }
    /// At most 3 decorative lines, matching the mock — deliberately NOT one
    /// per hidden member: past 3 the strip stops being a legible "there is
    /// more behind this" hint and starts being clutter, the same reasoning
    /// `Fleet.groupSetFragmentationThreshold` applies one level up.
    private var peekLineCount: Int { min(restMembers.count, 3) }

    /// `nil` under Reduce Motion: SwiftUI does not suppress an explicit
    /// `.animation`/`withAnimation` call for you (`QuotaBar` already
    /// documents this same rule), so every animated modifier below routes
    /// through this rather than `Tok.deckAnimation` directly. A `nil`
    /// animation still changes the STATE, so open/closed and hovered/not
    /// still read correctly — only the travel between them, not the states
    /// themselves, ever depends on this: the bridge's "keep the states and
    /// drop the travel" rule.
    private var animation: Animation? { reduceMotion ? nil : Tok.deckAnimation }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            header
            if isOpen {
                openStack
            } else {
                collapsedContent
            }
        }
        .padding(Tok.tightSpacing)
        .background(RoundedRectangle(cornerRadius: Tok.radiusLarge).fill(Tok.raised))
        .overlay(
            RoundedRectangle(cornerRadius: Tok.radiusLarge)
                .strokeBorder(Tok.hairlineStrong, lineWidth: Tok.hairlineWidth)
        )
        .onHover { isHovering = $0 }
    }

    private func toggleOpen() {
        withAnimation(animation) { isOpen.toggle() }
    }

    // MARK: Header

    /// The name/free-total/summary block is the ONLY part wrapped in the
    /// toggling `Button` — the group-actions menus beside it stay outside
    /// it. A `Menu` nested inside a `Button`'s label is a real AppKit
    /// hit-testing hazard (the outer button can steal the inner menu's
    /// click), so this is a bespoke layout rather than a reuse of
    /// ``AccountGroupSectionHeader`` wholesale; the restart/failure notice
    /// lines below duplicate that view's own logic for the same reason.
    private var header: some View {
        VStack(alignment: .leading, spacing: Tok.tightSpacing) {
            HStack(spacing: Tok.tightSpacing) {
                Button(action: toggleOpen) {
                    HStack(spacing: Tok.tightSpacing) {
                        if section.isReserved {
                            Image(systemName: "lock.fill")
                                .font(.caption)
                                .foregroundStyle(Tok.near)
                                .accessibilityLabel("Reserved")
                        }
                        VStack(alignment: .leading, spacing: Tok.rowLineSpacing) {
                            HStack(spacing: Tok.tightSpacing) {
                                Text(section.header)
                                    .font(Tok.secondaryFont.weight(.semibold))
                                    .lineLimit(1)
                                    .truncationMode(.middle)
                                Text("\(section.free)/\(section.total)")
                                    .font(Tok.secondaryDigitFont)
                                    .foregroundStyle(
                                        Tok.color(for: section.free == 0 ? FleetTally.Kind.spent : .ok)
                                    )
                            }
                            Text(section.summaryLine)
                                .font(Tok.detailFont)
                                .foregroundStyle(.secondary)
                        }
                        Spacer(minLength: 0)
                    }
                    .contentShape(Rectangle())
                }
                .buttonStyle(DeckHeaderButtonStyle())
                .accessibilityLabel(
                    "\(section.header), \(section.summaryLine). \(isOpen ? "Collapse" : "Expand")."
                )
                if !isUngrouped {
                    HStack(spacing: Tok.tightSpacing) {
                        ForEach(section.groups, id: \.self) { group in
                            GroupActionsMenu(
                                group: group,
                                allAccounts: allAccounts,
                                groupController: groupController,
                                confirmRemoveGroup: $confirmRemoveGroup
                            )
                        }
                    }
                }
            }
            if section.groups.contains(where: { groupController.needsRestart($0) }) {
                Text("restart the proxy to apply")
                    .font(Tok.detailFont)
                    .foregroundStyle(Tok.near)
            }
            if let failure = section.groups.compactMap({ groupController.failure(for: $0) }).first {
                Text(failure.summary)
                    .font(Tok.detailFont)
                    .foregroundStyle(Tok.spent)
            }
        }
        .accessibilityElement(children: .combine)
    }

    // MARK: Collapsed — fixed footprint, hover-safe (see the type doc-comment)

    private var collapsedContent: some View {
        VStack(alignment: .leading, spacing: 0) {
            if let topMember {
                registeredAccountRow(topMember)
                    .scaleEffect(reduceMotion ? 1 : (isHovering ? 1.008 : 1))
                    .shadow(
                        color: .black.opacity(isHovering ? 0.28 : 0),
                        radius: isHovering ? 10 : 0,
                        y: isHovering ? 4 : 0
                    )
            }
            peekIndicator
        }
        .padding(.top, Tok.tightSpacing)
        .animation(animation, value: isHovering)
    }

    /// The "there is more behind this" hint — a constant strip of short
    /// bars, present whenever the section has more than one member,
    /// regardless of hover. Hover only raises their opacity, hinting the
    /// outcome of a click without moving anything (see the type doc-comment
    /// on why this must never resize).
    @ViewBuilder
    private var peekIndicator: some View {
        if peekLineCount > 0 {
            VStack(alignment: .leading, spacing: 3) {
                ForEach(0..<peekLineCount, id: \.self) { index in
                    Capsule()
                        .fill(Tok.hairlineStrong)
                        .frame(height: 2)
                        .frame(maxWidth: .infinity)
                        .padding(.leading, CGFloat(index) * 5)
                }
            }
            .padding(.top, Tok.tightSpacing)
            .opacity(isHovering ? 0.42 : 0.16)
        }
    }

    // MARK: Open — the real per-member list

    private var openStack: some View {
        VStack(alignment: .leading, spacing: Tok.rowSpacing) {
            ForEach(section.members, id: \.id) { member in
                memberRow(member)
            }
        }
        .padding(.top, Tok.tightSpacing)
    }

    /// A hovered row lifts; every other row in the SAME open card recedes
    /// slightly, so the eye stays on the pointed-at row — the bridge's own
    /// "dock" description. `hoveredMemberID` is this card's own state, so
    /// two open cards never fight over which row is "the" hovered one.
    private func memberRow(_ member: Account) -> some View {
        let isHoveredRow = hoveredMemberID == member.id
        let anyHover = hoveredMemberID != nil
        return registeredAccountRow(member)
            .scaleEffect(reduceMotion ? 1 : (isHoveredRow ? 1.012 : (anyHover ? 0.995 : 1)))
            .opacity(anyHover ? (isHoveredRow ? 1 : 0.72) : 1)
            .zIndex(isHoveredRow ? 1 : 0)
            .animation(animation, value: hoveredMemberID)
            .onHover { hovering in
                if hovering {
                    hoveredMemberID = member.id
                } else if hoveredMemberID == member.id {
                    hoveredMemberID = nil
                }
            }
    }

    /// One member's ``AccountRow``, wrapped with the same `RowHeightsKey`
    /// registration `FleetView.accountRow(_:fleet:showGroupChips:)` attaches
    /// to every other row in the panel — mirrored here rather than shared,
    /// since the two call sites differ in exactly one thing (this one never
    /// draws group chips: the card's own header already says what they
    /// would). Used by both ``collapsedContent`` (``topMember`` only) and
    /// ``openStack`` (every member) — see the type doc-comment for why only
    /// the rows actually in flow register a height at all.
    private func registeredAccountRow(_ member: Account) -> some View {
        AccountRow(
            account: member,
            countersAreStructural: countersAreStructural,
            accounts: accounts,
            control: control,
            onChanged: onChanged,
            onRelogin: { onRelogin(member.name) },
            showGroupChips: false,
            groupController: groupController,
            allAccounts: allAccounts
        )
        .background(
            GeometryReader { proxy in
                Color.clear.preference(
                    key: RowHeightsKey.self, value: [member.id: proxy.size.height])
            }
        )
    }
}

/// Press feedback for the deck-card header's toggling `Button`.
/// `configuration.isPressed` reflects the pointer being DOWN, not the tap
/// having completed — SwiftUI updates it on press before `action` fires on
/// release — which is what satisfies the bridge's "respond on press, not
/// release" rule for the VISIBLE feedback while leaving the actual `isOpen`
/// commit on release, the same moment every other button in this panel
/// commits.
private struct DeckHeaderButtonStyle: ButtonStyle {
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .scaleEffect(reduceMotion ? 1 : (configuration.isPressed ? 0.99 : 1))
    }
}
