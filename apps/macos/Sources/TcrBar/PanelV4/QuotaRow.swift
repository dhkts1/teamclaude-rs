import SwiftUI
import TcrBarCore

/// One quota window, as a card draws it — the label, the value and the reset
/// every quota window carries. Named once here rather than nested in
/// ``AccountCard`` so both Compact and Comfortable build their rows from the
/// one type, instead of one of them re-deriving it.
struct QuotaWindowSpec {
    let label: String
    let value: Double?
    let tint: QuotaBarTintSource
    let resetAtMs: Int64?
}

/// What ``QuotaBarTintSource`` means for a bar — so the near/spent rule can
/// only be written once. See ``QuotaRow/role`` and ``QuotaRow/fillTint`` (retired in
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
    /// An optional figure parked at the END of this row, where the pre-v4 card
    /// drew the account's cost and output tokens. Carrying it here rather than
    /// on a line of its own is worth ~20 pt a card, which is most of the 28 pt
    /// the v4 card grew (measured 2026-09-13: 63 pt -> 91 pt for the same two
    /// windows). Only the FIRST window gets one; a figure repeated per row
    /// would say three different things about one account.
    var trailing: String? = nil
    /// Set on EVERY row of a card that carries a cost figure, including the
    /// rows that do not draw one, so all the bars keep one width.
    var trailingReserved: Bool = false
    /// What a hover says about ``trailing``. The inline figure is abbreviated
    /// to fit beside a bar; the full phrase it abbreviates stays one hover
    /// away rather than being lost with the row it used to occupy.
    var trailingHelp: String? = nil
    /// The tail text's colour. `nil` (the default) draws `Tok.mute`, the
    /// figure's ordinary colour — the cost tail and the plan name both stay
    /// neutral. The Fable tail passes its own window's tint
    /// (``Account/fableBarTintSource``) so a near-empty Fable window is still
    /// amber or red at a glance, the way its caption used to read.
    var trailingTint: Color? = nil

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
            // FIXED width, reserved on EVERY row whether or not this one has
            // a live caption — an auto-width column here is what made alike
            // bars draw at different lengths depending on which row happened
            // to carry the longer reset string (Gil, 2026-09-13: "not
            // aligned nicely").
            Text(QuotaFormat.resetCaption(resetAtMs: resetAtMs, now: now) ?? "")
                .font(V4.font(V4.muteSize))
                .foregroundStyle(captionTint)
                .lineLimit(1)
                .frame(width: V4.resetCaptionWidth, alignment: .trailing)
            if trailingReserved {
                // `.q .tail{border-left:1px solid var(--line)}` — one aligned
                // divider on every row that reserves the column, never a
                // floating `·`, so the boundary lines up whether or not the
                // row beside it draws a caption. Drawn even when `trailing`
                // is empty (round 2's Fable-less row 1): the column's LEFT
                // edge is a fact about the row, not about its content.
                Rectangle()
                    .fill(Tok.cardLine)
                    .frame(width: V4.panelBorderWidth)
                    .frame(maxHeight: .infinity)
                Text(trailing ?? "")
                    .font(V4.font(V4.muteSize))
                    .foregroundStyle(trailingTint ?? Tok.mute)
                    .lineLimit(1)
                    .truncationMode(.middle)
                    .frame(width: V4.usageTailWidth, alignment: .trailing)
                    .help(trailingHelp ?? trailing ?? "")
                    .accessibilityHidden(trailing == nil)
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
