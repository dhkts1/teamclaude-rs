import SwiftUI
import TcrBarCore

/// One quota window, as a card draws it — the label, the value and the reset
/// every quota window carries, whichever shape the card puts it in
/// (``QuotaRow``'s three-column grid, or ``DenseQuotaLine``'s one line).
///
/// Named once here rather than nested in ``AccountCard`` so both shapes build
/// their windows from the one type, instead of one of them re-deriving it.
struct QuotaWindowSpec {
    let label: String
    let value: Double?
    let tint: QuotaBarTintSource
    let resetAtMs: Int64?
}

/// What ``QuotaBarTintSource`` means for a bar, wherever one is drawn — shared
/// by ``QuotaRow`` and ``DenseQuotaLine`` so the near/spent rule can only be
/// written once. See ``QuotaRow/role`` and ``QuotaRow/fillTint`` (retired in
/// favour of this) for the reasoning: `.unmeasured` draws no fill at all,
/// `.measuredWithoutState` draws the sheet's neutral grey, and the Fable
/// window's own tint never borrows the composite `quotaState`.
extension QuotaBarTintSource {
    /// The window's own state, or `nil` when the server stated none — for
    /// EITHER absence, nothing measured or measured with no band, because a
    /// state word is what neither of them has.
    var role: QuotaState? {
        switch self {
        case .unmeasured, .measuredWithoutState: return nil
        case .state(let state): return state
        }
    }

    /// The fill's colour, or `nil` for a window with no reading — which draws
    /// NO fill at all, not a coloured sliver.
    var fillColor: Color? {
        switch self {
        case .unmeasured: return nil
        case .measuredWithoutState: return Tok.mute
        case .state(.near): return Tok.near
        case .state(.spent): return Tok.spent
        case .state(.ok): return Tok.ok
        case .state(.unknown): return Tok.mute
        }
    }
}

/// `.q` — one quota window: a 24 pt label, the bar taking every remaining point,
/// a 40 pt right-aligned percentage, then the reset caption.
///
/// Three things here are the fix for a measured defect rather than a
/// transcription:
///
///  - The bar is the grid's `1fr` column, so it FILLS the row (170 pt on this
///    panel). The pre-v4 bar was a fixed 72 pt, which made a 98 % window and a
///    30 % one look like similar amounts of ink.
///  - The percentage sits in its own fixed 40 pt column, right-aligned, so two
///    windows' digits line up and every `resets …` starts at one x.
///  - The window's verdict is SPOKEN, not only tinted. The label names the
///    window and the value carries the percentage, the state word and the
///    reset countdown — an `accessibilityLabel` carrying the percentage
///    overrode the combined children, so the `resets 3d 22h` caption and the
///    near/spent state reached a listener through nothing at all.
///  - The fill is a STATUS colour, taken from the window's own state
///    (`ok` / `warn` at the server's near threshold / `bad` over it) and never
///    from the enclosing group's identity colour. A 98 % window used to draw in
///    its group's violet beside an OK pill.
struct QuotaRow: View {
    let label: String
    /// `nil` is an unmeasured window: an explicit empty track, never a zero-width
    /// fill, because a zero reading and an absent one mean opposite things.
    let value: Double?
    /// What the bar's colour is allowed to be read from —
    /// ``Account/quotaBarTintSource(for:)`` and nothing else. That function is
    /// the one place that knows "no reading" and "old server, borrow the
    /// composite state" are different facts; re-deriving it here from
    /// `fiveHourState ?? quotaState` is the exact bug its doc-comment records.
    let tint: QuotaBarTintSource
    let resetAtMs: Int64?
    let now: Date

    private var fraction: Double {
        switch QuotaFormat.barFill(value) {
        case .measured(let v): return v
        case .unmeasured: return 0
        }
    }

    /// The window's own state, or `nil` when the server stated none —
    /// ``QuotaBarTintSource/role``, and nothing re-derived here. `.unknown` is
    /// a token THIS build cannot name, not a missing reading: the panel has no
    /// copy of the server's near-limit threshold, so it may not invent a band
    /// for it. It draws the sheet's own `.bar i.neutral` grey. What it must
    /// never do is what the first v4 render did — fall through to the
    /// `unmeasured` violet and paint a 98 % window the same colour as an
    /// account nothing has ever measured.
    private var role: QuotaState? { tint.role }

    /// The fill's colour, or `nil` for a window with no reading — which draws
    /// NO fill at all, not a coloured sliver. ``QuotaBarTintSource/fillColor``.
    ///
    /// `V4.barMinWidth` exists so a 0.4 % window is still visible as a mark
    /// rather than nothing. Applied to an UNMEASURED window it painted 2 pt of
    /// `Tok.unmeasured` at the left of an empty track, which says "a little
    /// has been spent" about an account nothing has ever probed — the one
    /// reading this row exists to distinguish from a real zero, drawn as a
    /// real zero. The empty track and the `n/a` percentage beside it are the
    /// whole statement.
    private var fillTint: Color? { tint.fillColor }

    /// `.resets.warn` — the caption turns amber on the same threshold the bar
    /// does, so the row does not say "nearly out" in one place and stay neutral
    /// in the other.
    private var captionTint: Color {
        switch role {
        case .some(.near): return Tok.near
        case .some(.spent): return Tok.spent
        default: return Tok.mute
        }
    }

    var body: some View {
        HStack(spacing: V4.quotaGap) {
            Text(label)
                .font(V4.font(V4.dimSize))
                .foregroundStyle(Tok.dim)
                .frame(width: V4.quotaLabelWidth, alignment: .leading)
            GeometryReader { proxy in
                ZStack(alignment: .leading) {
                    RoundedRectangle(cornerRadius: V4.barRadius)
                        .fill(Tok.ink.opacity(V4.barTrackAlpha))
                    if let fillTint {
                        RoundedRectangle(cornerRadius: V4.barRadius)
                            .fill(fillTint)
                            .frame(width: max(V4.barMinWidth, proxy.size.width * fraction))
                    }
                }
            }
            .frame(height: V4.barHeight)
            Text(QuotaFormat.percent(value))
                .font(V4.font(V4.dimSize))
                .foregroundStyle(Tok.dim)
                .frame(width: V4.quotaPercentWidth, alignment: .trailing)
            if let caption = QuotaFormat.resetsCaption(resetAtMs: resetAtMs, now: now) {
                Text(caption)
                    .font(V4.font(V4.muteSize))
                    .foregroundStyle(captionTint)
                    .fixedSize()
            }
        }
        .frame(minHeight: V4.lineHeight(V4.dimSize))
        .padding(.top, V4.quotaMarginTop)
        .accessibilityElement(children: .combine)
        .accessibilityLabel("\(label) window")
        .accessibilityValue(
            QuotaFormat.spokenWindowValue(
                value: value, state: role, resetAtMs: resetAtMs, now: now))
    }
}

/// Compact's quota block: every window ``AccountCard`` would otherwise draw as
/// one ``QuotaRow`` each, folded onto one line — `5h ▮▮▮▯▯ 19% · 7d ▮▯▯▯▯ 3% ·
/// fable ▯▯▯▯▯ 0%` (Gil, 2026-09-13: "its way more than what was before,
/// recheck"; `data/plans/dense-quota-bridge.md`).
///
/// The restored `fable` row plus three full ``QuotaRow`` lines cost Compact
/// +42 pt over its own prior two-row card (222 pt against 180) — a shorter
/// card fixes that, not a smaller font, so this draws the SAME bar tints and
/// near/spent rules as ``QuotaRow`` (via ``QuotaBarTintSource``) at a fixed
/// 40 pt bar width instead of one that fills the row.
///
/// What the multi-row shape carried that one line has no room for — the reset
/// caption per window — moves to this line's `.help` tooltip and to its
/// combined accessibility value, both built from
/// ``QuotaFormat/denseLineSpokenValue(windows:now:)`` so nothing spoken is
/// lost, only re-homed. A window the server does not report is absent from
/// the line entirely, never a placeholder `n/a` chip.
struct DenseQuotaLine: View {
    let windows: [QuotaWindowSpec]
    let now: Date

    private var spokenValue: String {
        QuotaFormat.denseLineSpokenValue(
            windows: windows.map {
                (label: $0.label, value: $0.value, state: $0.tint.role, resetAtMs: $0.resetAtMs)
            }, now: now)
    }

    var body: some View {
        // Two candidates, not a shrink-to-fit: `ViewThatFits` picks the first
        // that measures within 372 pt, so three windows' labels shrink to
        // `mute` size together, before any label truncates — never the last
        // window alone going small while the first two stay `dim`.
        ViewThatFits(in: .horizontal) {
            line(labelSize: V4.dimSize)
            line(labelSize: V4.muteSize)
        }
        .frame(minHeight: V4.lineHeight(V4.dimSize))
        .padding(.top, V4.quotaMarginTop)
        .help(spokenValue)
        .accessibilityElement(children: .ignore)
        .accessibilityLabel("Quota")
        .accessibilityValue(spokenValue)
    }

    private func line(labelSize: CGFloat) -> some View {
        HStack(spacing: V4.quotaGap) {
            ForEach(Array(windows.enumerated()), id: \.offset) { index, window in
                if index > 0 {
                    Text("·")
                        .font(V4.font(labelSize))
                        .foregroundStyle(Tok.mute)
                }
                chip(window, labelSize: labelSize)
            }
        }
    }

    private func chip(_ window: QuotaWindowSpec, labelSize: CGFloat) -> some View {
        HStack(spacing: V4.quotaGap) {
            Text(window.label)
                .font(V4.font(labelSize))
                .foregroundStyle(Tok.dim)
            ZStack(alignment: .leading) {
                RoundedRectangle(cornerRadius: V4.barRadius)
                    .fill(Tok.ink.opacity(V4.barTrackAlpha))
                if let fillColor = window.tint.fillColor {
                    RoundedRectangle(cornerRadius: V4.barRadius)
                        .fill(fillColor)
                        .frame(width: max(V4.barMinWidth, V4.denseBarWidth * fraction(window.value)))
                }
            }
            .frame(width: V4.denseBarWidth, height: V4.barHeight)
            Text(QuotaFormat.percent(window.value))
                .font(V4.font(labelSize))
                .foregroundStyle(Tok.dim)
        }
    }

    private func fraction(_ value: Double?) -> Double {
        switch QuotaFormat.barFill(value) {
        case .measured(let v): return v
        case .unmeasured: return 0
        }
    }
}
