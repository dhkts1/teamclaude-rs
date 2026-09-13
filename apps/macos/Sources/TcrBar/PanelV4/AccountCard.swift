import SwiftUI
import TcrBarCore

/// One account, as the mockup's Accounts panel draws it.
///
/// Two shapes, both cards — the group members are NOT bare rows. Measured, the
/// mockup gives each member a card: `rgba(255,255,255,.045)`, radius 8, ~42 pt
/// tall, 6 pt apart, inset inside the group's 8 pt padding. The pre-v4 panel drew
/// them as unfilled 29 pt rows whose text started flush against the group stroke.
///
///  - `.full`: name and pills, the plan line, then one `.q` row per quota window.
///  - `.compact`: one row — name, its plan inline in `mute`, and the state pill.
///    What a card inside a group draws, where the group's own legend already
///    carries the context the plan line would repeat.
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
                        V4Pill(text: statePillText, role: statePillRole, help: account.stateHelp)
                    }
                }
                .accessibilityElement(children: .combine)
                // The actions sit OUTSIDE that element, so they stay their own
                // focusable children. Combining them in would have made the
                // card one stop and taken every per-account action with it.
                actions()
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
                    trailingHelp: index == 1 ? row1TrailingHelp : planLine,
                    trailingTint: index == 1 ? fableTailTint : nil)
            }
        }
        // `.contain` WITH a label. Without one the container has no accessible
        // name, so it cannot take focus and a user arriving at the card is told
        // nothing about which account they have arrived at.
        .accessibilityElement(children: .contain)
        .accessibilityLabel(account.cardSummaryLabel(now: now, isControl: isControl))
    }

    @ViewBuilder
    private var nameRow: some View {
        if shape == .compact, let plan = account.plan, !plan.isEmpty {
            // `.name .mute` — the plan sits INSIDE the name span, so the row
            // reads as one subject with a qualifier. It is an inline span, not a
            // column: when the pair does not fit, the browser WRAPS it and the
            // card grows a line (the mockup's own `henry1@example.com` /
            // `Team Standard`). `ViewThatFits` is that wrap. The first v4 render
            // had no second candidate and truncated the ADDRESS instead —
            // "henry1@exam…" — which is the one string on the row that has to
            // stay readable.
            ViewThatFits(in: .horizontal) {
                HStack(spacing: V4.tabGap) {
                    NameText(text: account.name)
                    MuteText(text: plan)
                }
                VStack(alignment: .leading, spacing: 0) {
                    NameText(text: account.name)
                    MuteText(text: plan)
                        .frame(minHeight: V4.rowLineHeight, alignment: .leading)
                }
            }
        } else {
            NameText(text: account.name)
        }
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

    /// "Max 20x · $540 · 1.5M output tokens this week" — F8's plan line: the
    /// plan, what it spent, and what it produced, in full words.
    ///
    /// The pre-v4 line abbreviated the tail to "1.5M out" (`delta-list.md` #33)
    /// on a card 355 pt wide, where the full phrase fits. Built from the same
    /// ``QuotaFormat`` figures ``Account/windowUsageLabel`` uses — the same
    /// numbers, spelled for a card that has the room.
    /// The plan line's figures, abbreviated to sit at the end of the first
    /// quota row: `"$5.61 · 12k out"`. The pre-v4 card drew exactly this, in
    /// exactly this place, and the v4 card's full-width `planLine` above the
    /// bars is what made the card 28 pt taller for the same content. The full
    /// phrase, plan name included, is the hover.
    /// What the reserved right-hand column says on row `index`.
    ///
    /// The first row carries the money and tokens. The second carries the
    /// Fable weekly figure when this account has one (``Account/fableTailLabel(now:sevenDayResetAtMs:)``),
    /// which is where the pre-v4 card drew it too (Gil, 2026-09-13: "no like
    /// we had both … align it like we had before") — never both at once, so a
    /// Fable account's plan name is one hover away (``row1TrailingHelp``)
    /// rather than a second line. Falls back to the plan name, the pre-Fable
    /// behaviour, for every account this window was never learned for.
    private func rowTail(_ index: Int) -> String? {
        switch index {
        case 0: return usageTail
        case 1:
            if account.sevenDayOi != nil {
                return account.fableTailLabel(now: now, sevenDayResetAtMs: account.sevenDayResetAtMs)
            }
            // A grouped card already carries the plan INSIDE its name row
            // (``nameRow``), so repeating it here would print it twice on the
            // one card that is short of width.
            return shape == .compact ? nil : planName
        default: return nil
        }
    }

    /// The row-2 tail's colour: the Fable window's own tint
    /// (``Account/fableBarTintSource``) so a near-empty Fable window is still
    /// amber or red at a glance, never `quotaBarTintSource(for:)` — that
    /// function's old-server fallback borrows the composite `quotaState`,
    /// which for this window would be a reading of something else entirely.
    /// `nil` when there is no Fable figure, which falls back to `QuotaRow`'s
    /// own `Tok.mute` for the plan name it draws instead.
    private var fableTailTint: Color? {
        account.sevenDayOi != nil ? account.fableBarTintSource.fillColor : nil
    }

    /// The row-2 tail's hover text: the full ``Account/fableWeeklyLabel(now:)``
    /// plus the plan name (`"fable 72% · in 3d 18h · Max 20x"`), so the plan
    /// is one hover away rather than lost when the tail carries Fable
    /// instead of it. Falls back to ``planLine`` when there is no Fable
    /// figure, matching what row 2 actually shows.
    private var row1TrailingHelp: String? {
        guard account.sevenDayOi != nil, let fable = account.fableWeeklyLabel(now: now) else {
            return planLine
        }
        guard let plan = planName else { return fable }
        return "\(fable) · \(plan)"
    }

    /// `"Max 20x"` — the plan, on its own, for the row-2 tail.
    private var planName: String? {
        guard let plan = account.plan, !plan.isEmpty else { return nil }
        return plan
    }

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

    private var planLine: String? {
        var parts: [String] = []
        if let plan = account.plan, !plan.isEmpty { parts.append(plan) }
        if let usage = account.usage {
            let bucket = usage.windowOrToday
            let span = usage.windowOrTodaySpan == .day ? "today" : "this week"
            if let cost = bucket.measuredCost {
                // A partially priced bucket keeps its `+`: the figure is a floor.
                parts.append(QuotaFormat.usd(cost) + (bucket.unpricedRequests > 0 ? "+" : ""))
            }
            parts.append("\(QuotaFormat.tokens(bucket.outputTokens)) output tokens \(span)")
        }
        return parts.isEmpty ? nil : parts.joined(separator: " · ")
    }

    /// The windows that get a BAR ROW: the rolling session window and the
    /// weekly one, and only those two.
    ///
    /// `fable` is a SEPARATE window with a separate reset, gating Fable
    /// requests alone (`docs/cli.md`, "The weekly quota pair on `--json`"): a
    /// non-Fable request never checks it, and `held[]`/`quotaState` never
    /// reflect it — so it cannot be read off the `7d` bar beside it, and it
    /// gets no bar row of its own. It is drawn in the SAME trailing column as
    /// the cost figure and the plan name (``rowTail(_:)``), on the 7d row,
    /// which is where the pre-v4 card drew it too (Gil, 2026-09-13: "no like
    /// we had both … align it like we had before"). A row costs every card a
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
