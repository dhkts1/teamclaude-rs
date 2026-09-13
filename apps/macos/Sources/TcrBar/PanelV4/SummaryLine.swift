import SwiftUI
import TcrBarCore

/// `.sum` — the sentence directly under the title. 15 pt, `dim`, numbers at 600,
/// and the role words in their own colours.
///
/// Concatenated `Text`, never an `HStack`: a stack cannot wrap, so it would
/// truncate the tail of the sentence instead of flowing it onto a second line,
/// and the tail is where the money is.
struct SummaryLine: View {
    /// The runs to draw, in order. Each is a fragment with its own colour and
    /// weight; the view joins them with the sheet's own `·` separator in `mute`.
    struct Run {
        let text: String
        var tint: Color = Tok.dim
        /// `.sum b` / `.sum .ok` — a figure is 600, prose is 400.
        var emphasised: Bool = false
    }

    /// One or two lines. The Accounts tab draws two — the capacity breakdown,
    /// then spend and cache — because a single wrapped sentence of five clauses
    /// reads as one long number (`data/plans/panel-v4-migration-bridge.md`'s own
    /// `.sum` row).
    let lines: [[Run]]
    var accessibilityText: String?

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            ForEach(Array(lines.enumerated()), id: \.offset) { _, runs in
                text(for: runs)
                    .fixedSize(horizontal: false, vertical: true)
            }
        }
        .padding(.horizontal, V4.summaryPaddingSide)
        .padding(.bottom, V4.summaryPaddingBottom)
        .frame(maxWidth: .infinity, alignment: .leading)
        .accessibilityElement(children: .combine)
        .accessibilityLabel(
            accessibilityText
                ?? lines.map { $0.map(\.text).joined(separator: ", ") }.joined(separator: ". "))
    }

    private func text(for runs: [Run]) -> Text {
        var out = Text("")
        for (index, run) in runs.enumerated() {
            if index > 0 {
                out =
                    out
                    + Text(" · ")
                    .font(V4.font(V4.summarySize))
                    .foregroundColor(Tok.mute)
            }
            out =
                out
                + Text(run.text)
                .font(V4.font(V4.summarySize, run.emphasised ? .semibold : .regular))
                .foregroundColor(run.tint)
        }
        return out
    }
}

extension SummaryLine {
    /// The Accounts tab's two lines, built from the fleet's own tallies — never
    /// re-counted here. Line one is the population breakdown (`9 ready · 3 near
    /// limit · 1 unmeasured`), line two is what it cost (`$41.80 today · cache
    /// 95%`).
    ///
    /// ``Fleet/sentenceBreakdown``, not ``Fleet/breakdown``: the latter leaves
    /// out the unmeasured and need-re-login buckets for a header that names them
    /// in a clause this line does not have.
    static func accounts(_ fleet: Fleet) -> SummaryLine {
        var first: [Run] = fleet.sentenceBreakdown.map { tally in
            Run(text: tally.sentenceLabel, tint: Tok.color(for: tally.kind), emphasised: true)
        }
        if first.isEmpty {
            first = [Run(text: fleet.capacitySummary, tint: Tok.color(for: fleet.capacityState))]
        }
        var second: [Run] = []
        if fleet.hasUsage {
            second.append(
                Run(text: "\(QuotaFormat.usd(fleet.todayCost)) today", emphasised: true))
            second.append(
                Run(text: "cache \(QuotaFormat.percent(fleet.todayCacheHitRatio))"))
            if let unpriced = fleet.todayUnpricedRequests, unpriced > 0 {
                // Kept even though the mockup has no such case: it says the
                // figure beside it is a FLOOR, and dropping it would make a
                // partially-priced day read as a measured one.
                second.append(Run(text: "\(unpriced) unpriced today", tint: Tok.mute))
            }
        }
        return SummaryLine(
            lines: second.isEmpty ? [first] : [first, second],
            accessibilityText: "\(fleet.capacitySummary). \(fleet.breakdownLabel)")
    }
}
