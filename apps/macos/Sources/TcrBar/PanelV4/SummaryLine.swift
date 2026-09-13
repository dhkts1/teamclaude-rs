import SwiftUI
import TcrBarCore

/// `.sum` — the sentence directly under the title. 15 pt, `dim`, numbers at 600,
/// and the role words in their own colours.
///
/// Concatenated `Text`, never an `HStack`: a stack cannot wrap, so it would
/// truncate the tail of the sentence instead of flowing it onto a second line,
/// and the tail is where the money is.
struct SummaryLine: View {
    /// One clause of the sentence: a FIGURE and the words around it.
    ///
    /// The sheet weights the number, not the clause (`.sum b{font-weight:600}`
    /// wraps the digits alone). Bolding "9 ready" whole makes the line read as
    /// five headings; bolding "9" makes it read as a sentence with five numbers
    /// in it, which is what it is. Colour belongs to the clause, weight to the
    /// figure — so ``label`` is drawn at 400 in the SAME tint.
    struct Run {
        /// The digits, at 600.
        let figure: String
        /// The words, at 400. Before the figure when ``labelLeads`` (`cache 95%`),
        /// after it otherwise (`9 ready`, `$41.80 today`).
        var label: String = ""
        var labelLeads: Bool = false
        var tint: Color = Tok.dim

        var spoken: String {
            labelLeads ? "\(label) \(figure)" : "\(figure) \(label)"
        }
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
                    .lineSpacing(V4.lineSpacing(V4.summarySize))
                    .fixedSize(horizontal: false, vertical: true)
                    .frame(minHeight: V4.lineHeight(V4.summarySize), alignment: .leading)
            }
        }
        .padding(.horizontal, V4.summaryPaddingSide)
        .padding(.bottom, V4.summaryPaddingBottom)
        .frame(maxWidth: .infinity, alignment: .leading)
        .accessibilityElement(children: .combine)
        .accessibilityLabel(
            accessibilityText
                ?? lines.map { $0.map(\.spoken).joined(separator: ", ") }.joined(separator: ". "))
    }

    private func text(for runs: [Run]) -> Text {
        var out = Text("")
        for (index, run) in runs.enumerated() {
            if index > 0 {
                out = out + separator
            }
            let figure = fragment(run.figure, run.tint, .semibold)
            guard !run.label.isEmpty else {
                out = out + figure
                continue
            }
            let label = fragment(run.label, run.tint, .regular)
            let space = fragment(" ", run.tint, .regular)
            out =
                out + (run.labelLeads ? label + space + figure : figure + space + label)
        }
        return out
    }

    private var separator: Text {
        fragment(" · ", Tok.mute, .regular)
    }

    private func fragment(_ text: String, _ tint: Color, _ weight: Font.Weight) -> Text {
        Text(text)
            .font(V4.font(V4.summarySize, weight))
            .foregroundColor(tint)
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
    /// `.sum .ok{color:var(--ok);font-weight:600}`, `.sum .warn{color:var(--warn)}`,
    /// `.sum .bad{color:var(--bad)}` — and nothing else. A bucket with no role
    /// class stays `dim` at 400: the sheet colours what the operator can ACT on,
    /// and colouring every clause is how a sentence stops reading as a sentence.
    /// Only `ok` carries the weight.
    private static func tint(_ kind: FleetTally.Kind) -> Color {
        switch kind {
        case .ok: return Tok.ok
        case .near: return Tok.near
        case .spent, .needsRelogin: return Tok.spent
        case .unknown, .unmeasured, .disabled: return Tok.dim
        }
    }

    static func accounts(_ fleet: Fleet) -> SummaryLine {
        var first: [Run] = fleet.sentenceBreakdown.map { tally in
            Run(figure: "\(tally.count)", label: tally.kind.phrase, tint: tint(tally.kind))
        }
        if first.isEmpty {
            first = [
                Run(figure: fleet.capacitySummary, tint: Tok.color(for: fleet.capacityState))
            ]
        }
        var second: [Run] = []
        if fleet.hasUsage {
            // `dim`, not white: the mockup's money clause is `.nw`, which sets
            // wrapping and nothing else — `.sum b{color:#fff}` is a rule for the
            // OTHER two tabs' summaries, where the figure is the subject of the
            // sentence. Here it is one clause of five.
            second.append(
                Run(figure: QuotaFormat.usd(fleet.todayCost), label: "today"))
            second.append(
                Run(
                    figure: QuotaFormat.percent(fleet.todayCacheHitRatio), label: "cache",
                    labelLeads: true))
            if let unpriced = fleet.todayUnpricedRequests, unpriced > 0 {
                // Kept even though the mockup has no such case: it says the
                // figure beside it is a FLOOR, and dropping it would make a
                // partially-priced day read as a measured one.
                second.append(
                    Run(figure: "\(unpriced)", label: "unpriced today", tint: Tok.mute))
            }
        }
        return SummaryLine(
            lines: second.isEmpty ? [first] : [first, second],
            accessibilityText: "\(fleet.capacitySummary). \(fleet.breakdownLabel)")
    }
}
