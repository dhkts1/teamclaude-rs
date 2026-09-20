import SwiftUI
import TcrBarCore

/// One account, as the mockup's Accounts panel draws it.
///
/// Two shapes, both cards — the group members are NOT bare rows. Measured, the
/// mockup gives each member a card: `rgba(255,255,255,.045)`, radius 8, ~42 pt
/// tall, 6 pt apart, inset inside the group's 8 pt padding. The pre-v4 panel drew
/// them as unfilled 29 pt rows whose text started flush against the group stroke.
///
///  - `.full`: name, pills and plan on one line, then one `.q` row per quota
///    window.
///  - `.compact`: the same one-line name row, and the state pill. What a card
///    inside a group draws, where the group's own legend already carries the
///    context a second line would repeat.
///
/// The trailing slot carries the account's own controls — the actions menu, and
/// `Re-login…` on a broken card. They used to be reachable by right-click ALONE
/// (`.contextMenu` was the card's only interaction), so every per-account
/// action — Re-login, Enable/Disable, Use as Control Account, Copy Access
/// Token, Mint Long-Lived Token, Delete Account, Remove from group, two of them
/// destructive — was unreachable from a keyboard, and a card reading NEEDS
/// RE-LOGIN offered no visible way to repair itself. The context menu stays as
/// the second route.
///
/// The pills are outlined, never filled, and the state pill agrees with the bars
/// below it: both are computed from the same ``FleetTally/Kind`` classifier, so a
/// card can no longer read OK over a 98 % bar.
struct AccountCard<Actions: View>: View {
    enum Shape {
        case full
        case compact
    }

    let account: Account
    var shape: Shape = .full
    let now: Date
    /// True when this account is the identity-bound control account
    /// (`tcr control --show`) — every quota figure the fleet draws is measured
    /// through it. Drawn as the card's own `CONTROL` pill rather than a
    /// standalone line above the tab: the pre-v4 line stated the fact once for
    /// the whole panel and required a reader to hold "which account" in their
    /// head while scanning the cards below it (review's card-pill ask).
    var isControl: Bool = false
    /// The Macs drawing on this account, from `tcr peer ls --json`'s `lentTo`
    ///, the LENDER's own file, so the line costs no wire call and says
    /// nothing a peer told us.
    ///
    /// Empty on an account inside no lease, which draws no line at all: that
    /// absence is how an operator tells the two states apart at a glance
    /// (decision row 13, and the mockup's scene 64 ledger).
    var lentTo: [PeerLentToEntry] = []
    /// Opens that Mac's sheet in Settings > Peers. The card's job is to say
    /// THAT the account is lent; how much is the sheet's.
    var onOpenLender: (String) -> Void = { _ in }
    /// Where this account's requests leave from, or `nil` when nothing
    /// reported it, which draws no row at all.
    ///
    /// Absent is the state every `tcr` in this tree is in: no read carries the
    /// account's `egress` keys yet. A picker defaulting to "This Mac" would be
    /// this panel asserting where traffic leaves, which is the one claim this
    /// control exists to make honestly.
    var exit: AccountExit? = nil
    /// The trusted Macs the picker may name.
    var exitPeers: [String] = []
    /// See ``AccountExitRow/peerRows``.
    var exitPeerRows: [PeerListDocument.PeerEntry] = []
    /// Still controls instead of a `Menu`, for `--render-states`.
    var snapshotMode: Bool = false
    var onChooseExit: (AccountExit.Route) -> Void = { _ in }
    var onToggleExitMust: (Bool) -> Void = { _ in }
    /// The card's visible per-account controls — the actions menu, and the
    /// re-login button on a broken account. A closure so ``AccountCard`` stays
    /// free of the controllers those controls are wired to: they are built from
    /// the ONE definition in `AccountRow`, and a second copy for the v4 card
    /// would be a second thing to keep in step with `tcr`'s subcommands.
    @ViewBuilder var actions: () -> Actions

    var body: some View {
        V4Card {
            HStack(spacing: V4.pillGap) {
                // The informational half of the header is ONE accessibility
                // element. VoiceOver walked roughly eight stops per card before
                // this — name, each pill, the plan line, each bar — to reach a
                // card that, being a `.contain` container with no label of its
                // own, could not be focused or summarised at any of them.
                V4Row {
                    nameRow
                } trailing: {
                    HStack(spacing: V4.pillGap) {
                        // Drawn first: a designation, read before either state
                        // word, the same order ``cardSummaryLabel`` speaks it.
                        if isControl {
                            V4Pill(
                                text: "Control",
                                help: "Every quota figure on this panel is measured through "
                                    + "\(account.name).")
                        }
                        if let rotation = rotationPillText {
                            V4Pill(text: rotation, help: account.rotationHelp)
                        }
                        // Not drawn while the waiting pill below is showing:
                        // a card whose exit Mac is down must not also read
                        // green OK at a glance.
                        if exitWaitingPillText == nil {
                            V4Pill(text: statePillText, role: statePillRole, help: account.stateHelp)
                        }
                    }
                }
                .accessibilityElement(children: .combine)
                // The actions sit OUTSIDE that element, so they stay their own
                // focusable children. Combining them in would have made the
                // card one stop and taken every per-account action with it.
                actions()
            }
            // An account must-locked to a down exit Mac must not read as
            // healthy on OK's word alone. Putting a third badge in the
            // header row beside OK, at the panel's real 372 pt width
            // (`V4.panelWidth`), crowds the account's own name off the row
            // entirely (`alice @example.com` clipped to `... M...`). Its own
            // line, right aligned under the header, keeps the pill the first
            // thing read after the name without taking the name's place.
            if let waiting = exitWaitingPillText {
                HStack(spacing: 0) {
                    Spacer(minLength: 0)
                    V4Pill(text: waiting, role: .warn, help: exitWaitingPillHelp)
                }
            }
            // One row per window in BOTH shapes, which is what the comment
            // here has claimed since #248 while the code drew them in one.
            // `if shape == .full` read as if it were the density switch it
            // sits beside in every other token (`V4.compact`), but
            // ``Shape/compact`` means something else entirely: a card inside a
            // group box. So every grouped account drew its name, its pills and
            // NOTHING ELSE — no 5h, no 7d, no model-scoped window — while the
            // router was rotating on exactly those numbers. Seven of Gil's
            // eighteen accounts are in a group (Gil, 2026-09-13: "why i dont
            // see any fable?").
            //
            // Compact used to fold these onto a single dense line (the fix for
            // a measured +42 pt over its own two-row card) — Gil saw that line
            // and preferred readable bars, so it draws the same rows, just at
            // Compact's own tighter density tokens (`V4.quotaLabelWidth`,
            // `V4.quotaMarginTop`, `V4.barHeight`).
            ForEach(Array(quotaWindows.enumerated()), id: \.element.label) {
                index, window in
                QuotaRow(
                    label: window.label, value: window.value, tint: window.tint,
                    resetAtMs: window.resetAtMs, now: now,
                    trailing: rowTail(index),
                    trailingReserved: usageTail != nil || rowTail(1) != nil,
                    trailingHelp: index == 1 ? row1TrailingHelp : usageTailHelp,
                    trailingTint: index == 1 ? fableTailTint : nil)
            }
            if let line = PeerLease.lentToLine(lentTo) {
                lentLine(line)
            }
            if let exit {
                AccountExitRow(
                    account: account.name, exit: exit, peers: exitPeers,
                    peerRows: exitPeerRows,
                    snapshotMode: snapshotMode, onChoose: onChooseExit,
                    onToggleMust: onToggleExitMust)
            }
        }
        // `.contain` WITH a label. Without one the container has no accessible
        // name, so it cannot take focus and a user arriving at the card is told
        // nothing about which account they have arrived at.
        .accessibilityElement(children: .contain)
        .accessibilityLabel(account.cardSummaryLabel(now: now, isControl: isControl))
    }

    /// Decision row 13's line, under the meters: who is drawing on this
    /// account.
    ///
    /// **A control as well as a readout.** Pressing it opens the first named
    /// Mac's sheet, where the amounts and the Revoke live, which is the
    /// mockup's own behaviour for scene 64 and the reason this is a `Button`
    /// and not a `Text`. The chevron is what says so.
    ///
    /// It wears the reserved plaintext hue, because being inside a lease means
    /// another Mac may spend this account's allowance and read what it sends.
    /// The hue is a second channel: the sentence carries the meaning.
    private func lentLine(_ line: String) -> some View {
        Button {
            guard let first = lentTo.first?.peer else { return }
            onOpenLender(first)
        } label: {
            HStack(spacing: V4.pillGap) {
                Text(line)
                    .font(V4.font(V4.muteSize))
                    .foregroundStyle(Tok.unknown)
                    .lineLimit(1)
                    .truncationMode(.tail)
                Spacer(minLength: 0)
                Image(systemName: "chevron.right")
                    .font(.system(size: V4.muteSize - 1, weight: .semibold))
                    .foregroundStyle(Tok.mute)
            }
            .padding(.top, V4.quotaMarginTop)
        }
        .buttonStyle(.plain)
        .help(
            "Another Mac may spend this account's allowance and reads what it sends. Opens "
                + "that Mac's sheet in Settings > Peers, where the amount and Revoke are."
        )
        .accessibilityLabel("\(line). Opens that Mac's settings sheet.")
    }

    /// Three pieces, ONE line, in BOTH shapes (`.name.acct` in the mockup):
    /// the local part at the name's usual weight, `@domain` at medium weight
    /// in ``Tok/dim`` — the ONLY piece allowed to truncate — then the plan in
    /// `mute`, never truncated. Split at the first `@`; ``Account/name`` may
    /// lack one, in which case the whole string is the local part and there
    /// is no domain span at all.
    ///
    /// Round 1 gave `.compact` a `ViewThatFits` that wrapped the plan onto a
    /// second line when the pair did not fit — that is what the mockup's own
    /// `henry1@example.com` / `Team Standard` two-line card shows, because
    /// round 1 did not restructure those two rows. Round 2 does: the card
    /// never grows a line for the plan, in either shape.
    ///
    /// WHICH piece gives way was decided on a render on 2026-09-13, the domain
    /// first and the plan never, and reopened by the owner on 2026-09-20 asking
    /// for the name's room back. The order is now plan, then domain, then the
    /// name: the name is the only piece that says WHICH account this card is,
    /// the plan is repeated across most of the fleet, and the domain is
    /// usually the same one twice over. A control account's card carries a
    /// second pill and is the case where the difference shows.
    @ViewBuilder
    private var nameRow: some View {
        HStack(alignment: .firstTextBaseline, spacing: V4.tabGap) {
            // NONE of these three takes `.fixedSize()` — that forces a view
            // to its ideal width regardless of what the row can actually
            // give it, which is the opposite of "never truncated": measured
            // on `01g-widest-row`, it overflowed the row's whole HStack and
            // corrupted the layout above it. `layoutPriority` is what orders
            // the three instead: `HStack` asks the lowest priority to shrink
            // first, so the plan gives way, then the domain, and the name
            // last.
            Text(localPart)
                .font(V4.font(V4.nameSize, .semibold))
                .tracking(V4.nameTracking)
                .foregroundStyle(Tok.ink)
                .lineLimit(1)
                .layoutPriority(1)
            if let domain {
                Text(domain)
                    .font(V4.font(V4.nameSize, .medium))
                    .foregroundStyle(Tok.dim)
                    .lineLimit(1)
                    .truncationMode(.tail)
                    .layoutPriority(-1)
            }
            if let plan = planName {
                MuteText(text: plan)
                    .layoutPriority(-2)
            }
        }
        .frame(minHeight: V4.lineHeight(V4.nameSize), alignment: .leading)
        .help(account.name)
        .accessibilityValue(account.name)
    }

    /// `"henry10"` — everything before the first `@`, or the whole name when
    /// it has none.
    private var localPart: String {
        guard let at = account.name.firstIndex(of: "@") else { return account.name }
        return String(account.name[account.name.startIndex..<at])
    }

    /// `"@example.com"`, `@` included so the split does not have to be undone
    /// by whatever draws it — or `nil` for a name with no `@` at all.
    private var domain: String? {
        guard let at = account.name.firstIndex(of: "@") else { return nil }
        return String(account.name[at...])
    }

    /// `Rotating` / `Group only` — ``Account/rotationLabel``, and nothing
    /// re-derived here.
    ///
    /// It used to be `!disabled && !isParkedByGroup && !isRejected`, which
    /// missed the dead-credential case entirely — a card read ROTATING beside
    /// NEEDS RE-LOGIN — and had no word for a reserved account at all.
    ///
    /// `"Rotating"` alone is dropped on a compact card, and `"Group only"` is
    /// not. A compact card is a card inside a group box, and what that box's
    /// legend says once for all of its members is whether the GROUP is parked —
    /// so repeating "rotating" per row is noise. It says nothing about the
    /// group being RESERVED, which is what "Group only" reports, and suppressing
    /// both left the one account the word describes with no word at all: the
    /// `research` group in `01-healthy` is reserved, and its member's card drew
    /// an unqualified OK.
    private var rotationPillText: String? {
        guard let rotation = account.rotation else { return nil }
        if shape == .compact, rotation == .rotating { return nil }
        return rotation.label
    }
    private var kind: FleetTally.Kind {
        FleetTally.Kind(account: account)
    }

    /// The header's second pill: `waiting for <peer>`, the same wording
    /// ``AccountExitRow`` draws under the picker, when this account is
    /// must-locked to a Mac that is down right now. `nil` on every other
    /// account, which draws no second pill at all.
    private var exitWaitingPillText: String? {
        exit?.waitingPill(peers: exitPeerRows)
    }

    /// The sentence behind the waiting pill, the same one
    /// ``AccountExitRow`` attaches to its own waiting line.
    private var exitWaitingPillHelp: String {
        "This account is pinned to that Mac and may not use another address, so its "
            + "requests wait until it is back."
    }

    private var statePillText: String {
        switch kind {
        case .ok: return "OK"
        case .near: return "Near"
        case .spent: return "Spent"
        case .unknown: return "Unknown"
        case .needsRelogin: return "Needs re-login"
        case .rejected: return "Rejected"
        case .unmeasured: return "Unmeasured"
        case .disabled: return "Parked"
        }
    }

    private var statePillRole: V4Pill.Role {
        switch kind {
        case .ok: return .ok
        case .near: return .warn
        case .spent, .needsRelogin, .rejected: return .bad
        case .unmeasured: return .info
        case .unknown, .disabled: return .neutral
        }
    }

    /// What the reserved right-hand column says on row `index`. The plan name
    /// left the tail entirely in round 2 — it is visible in ``nameRow`` now,
    /// in both shapes, so a second copy here would say it twice.
    ///
    /// Row 0 carries the money and tokens. Row 1 carries the Fable weekly
    /// figure (``Account/fableTailLabel``) when this account has one, which is
    /// where the pre-v4 card drew it too (Gil, 2026-09-13: "no like we had
    /// both … align it like we had before") — `nil`, an EMPTY column, for
    /// every account this window was never learned for.
    private func rowTail(_ index: Int) -> String? {
        switch index {
        case 0: return usageTail
        case 1: return account.fableTailLabel
        default: return nil
        }
    }

    /// The row-2 tail's colour: the Fable window's own tint
    /// (``Account/fableBarTintSource``) so a near-empty Fable window is still
    /// amber or red at a glance, never `quotaBarTintSource(for:)` — that
    /// function's old-server fallback borrows the composite `quotaState`,
    /// which for this window would be a reading of something else entirely.
    /// `nil` when there is no Fable figure — the column is empty, so there is
    /// nothing to tint.
    private var fableTailTint: Color? {
        account.sevenDayOi != nil ? account.fableBarTintSource.fillColor : nil
    }

    /// The row-2 tail's hover text: the full ``Account/fableWeeklyLabel(now:)``,
    /// reset included — `"fable 72% · in 3d 18h"`, the mockup's own tail
    /// title, with no plan appended, since the plan is already visible in
    /// ``nameRow``. `nil` when the column is empty, matching what row 2
    /// actually shows.
    private var row1TrailingHelp: String? {
        guard account.sevenDayOi != nil else { return nil }
        return account.fableWeeklyLabel(now: now)
    }

    /// `"Max 20x"` — the plan, on its own, for ``nameRow``.
    private var planName: String? {
        guard let plan = account.plan, !plan.isEmpty else { return nil }
        return plan
    }

    /// `"$540 · 1.5M"` — the mockup's own row-0 tail text, unabbreviated
    /// further: ``QuotaFormat/tokens(_:)`` already omits the `" out"` suffix,
    /// so nothing here needs to strip it.
    private var usageTail: String? {
        guard let usage = account.usage else { return nil }
        let bucket = usage.windowOrToday
        var parts: [String] = []
        if let cost = bucket.measuredCost {
            parts.append(QuotaFormat.usd(cost) + (bucket.unpricedRequests > 0 ? "+" : ""))
        }
        parts.append(QuotaFormat.tokens(bucket.outputTokens))
        return parts.isEmpty ? nil : parts.joined(separator: " · ")
    }

    /// The row-0 tail's hover: `"$540 · 1.5M output tokens this week"`, the
    /// mockup's own tail title — no plan, which is visible in ``nameRow``
    /// already and would otherwise say the same fact twice.
    private var usageTailHelp: String? {
        guard let usage = account.usage else { return nil }
        let bucket = usage.windowOrToday
        let span = usage.windowOrTodaySpan == .day ? "today" : "this week"
        var parts: [String] = []
        if let cost = bucket.measuredCost {
            // A partially priced bucket keeps its `+`: the figure is a floor.
            parts.append(QuotaFormat.usd(cost) + (bucket.unpricedRequests > 0 ? "+" : ""))
        }
        parts.append("\(QuotaFormat.tokens(bucket.outputTokens)) output tokens \(span)")
        return parts.joined(separator: " · ")
    }

    /// The windows that get a BAR ROW: the rolling session window and the
    /// weekly one, and only those two.
    ///
    /// `fable` is a SEPARATE window with a separate reset, gating Fable
    /// requests alone (`docs/cli.md`, "The weekly quota pair on `--json`"): a
    /// non-Fable request never checks it, and `held[]`/`quotaState` never
    /// reflect it — so it cannot be read off the `7d` bar beside it, and it
    /// gets no bar row of its own. It is drawn in the SAME trailing column as
    /// the cost figure (``rowTail(_:)``), on the 7d row, which is where the
    /// pre-v4 card drew it too (Gil, 2026-09-13: "no like we had both … align
    /// it like we had before"). A row costs every card a
    /// measured 21 pt; the tail column costs nothing extra, because it is
    /// already reserved for the figure above it.
    private var quotaWindows: [QuotaWindowSpec] {
        [
            QuotaWindowSpec(
                label: "5h", value: account.fiveHour,
                tint: account.quotaBarTintSource(for: .fiveHour),
                resetAtMs: account.fiveHourResetAtMs),
            QuotaWindowSpec(
                label: "7d", value: account.sevenDay,
                tint: account.quotaBarTintSource(for: .sevenDay),
                resetAtMs: account.sevenDayResetAtMs),
        ]
    }
}
