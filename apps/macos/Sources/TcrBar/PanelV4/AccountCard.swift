import SwiftUI
import TcrBarCore

/// One account, as the mockup's Accounts panel draws it.
///
/// Two shapes, both cards — the group members are NOT bare rows
/// (`/tmp/parity/delta-list.md` #22):
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
                    if shape == .full, isRotating {
                        V4Pill(text: "Rotating")
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
                        label: window.label, value: window.value, state: window.state,
                        resetAtMs: window.resetAtMs, now: now)
                }
            }
        }
        .accessibilityElement(children: .contain)
    }

    @ViewBuilder
    private var nameRow: some View {
        if shape == .compact, let plan = account.plan, !plan.isEmpty {
            // `.name .mute` — the plan sits INSIDE the name span in the mockup,
            // on one line, so the row reads as one subject with a qualifier.
            HStack(spacing: V4.tabGap) {
                NameText(text: account.name)
                MuteText(text: plan)
            }
        } else {
            NameText(text: account.name)
        }
    }

    /// `Rotating` — the account is in the pool right now. Suppressed on a
    /// compact card: every row inside a parked group is out of rotation and the
    /// legend says so once for all of them.
    private var isRotating: Bool {
        !account.disabled && !account.isParkedByGroup && !account.isRejected
    }

    private var kind: FleetTally.Kind {
        account.disabled ? .disabled : FleetTally.Kind(account: account)
    }

    private var statePillText: String {
        switch kind {
        case .ok: return "OK"
        case .near: return "Near"
        case .spent: return "Spent"
        case .unknown: return "Unknown"
        case .needsRelogin: return "Needs re-login"
        case .unmeasured: return "Unmeasured"
        case .disabled: return "Parked"
        }
    }

    private var statePillRole: V4Pill.Role {
        switch kind {
        case .ok: return .ok
        case .near: return .warn
        case .spent, .needsRelogin: return .bad
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
        let state: QuotaState?
        let resetAtMs: Int64?
    }

    /// The two windows the mockup's card draws. A window with no reading at all
    /// is still drawn — an empty track is a fact ("never measured"), and dropping
    /// the row would make a card that has never been probed look like one with
    /// nothing to report.
    private var quotaWindows: [Window] {
        [
            Window(
                label: "5h", value: account.fiveHour, state: account.fiveHourState,
                resetAtMs: account.fiveHourResetAtMs),
            Window(
                label: "7d", value: account.sevenDay, state: account.sevenDayState,
                resetAtMs: account.sevenDayResetAtMs),
        ]
    }
}
