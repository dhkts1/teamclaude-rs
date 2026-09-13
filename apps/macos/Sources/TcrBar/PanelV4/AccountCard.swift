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
/// The pills are outlined, never filled, and the state pill agrees with the bars
/// below it: both are computed from the same ``FleetTally/Kind`` classifier, so a
/// card can no longer read OK over a 98 % bar.
struct AccountCard: View {
    enum Shape {
        case full
        case compact
    }

    let account: Account
    var shape: Shape = .full
    let now: Date

    var body: some View {
        V4Card {
            V4Row {
                nameRow
            } trailing: {
                HStack(spacing: V4.pillGap) {
                    if shape == .full, let rotation = account.rotationLabel {
                        V4Pill(text: rotation)
                    }
                    V4Pill(text: statePillText, role: statePillRole)
                }
            }
            if shape == .full {
                if let plan = planLine {
                    MuteText(text: plan)
                }
                ForEach(quotaWindows, id: \.label) { window in
                    QuotaRow(
                        label: window.label, value: window.value, tint: window.tint,
                        resetAtMs: window.resetAtMs, now: now)
                }
            }
        }
        .accessibilityElement(children: .contain)
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
    /// re-derived here. Suppressed on a compact card: every row inside a parked
    /// group is out of rotation and the legend says so once for all of them.
    ///
    /// It used to be `!disabled && !isParkedByGroup && !isRejected`, which
    /// missed the dead-credential case entirely — a card read ROTATING beside
    /// NEEDS RE-LOGIN — and had no word for a reserved account at all.
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

    private struct Window {
        let label: String
        let value: Double?
        let tint: QuotaBarTintSource
        let resetAtMs: Int64?
    }

    /// The two windows the mockup's card draws. A window with no reading at all
    /// is still drawn — an empty track is a fact ("never measured"), and dropping
    /// the row would make a card that has never been probed look like one with
    /// nothing to report.
    private var quotaWindows: [Window] {
        [
            Window(
                label: "5h", value: account.fiveHour,
                tint: account.quotaBarTintSource(for: .fiveHour),
                resetAtMs: account.fiveHourResetAtMs),
            Window(
                label: "7d", value: account.sevenDay,
                tint: account.quotaBarTintSource(for: .sevenDay),
                resetAtMs: account.sevenDayResetAtMs),
        ]
    }
}
