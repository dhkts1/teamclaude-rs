import SwiftUI
import TcrBarCore

/// `.q` — one quota window: a 24 pt label, the bar taking every remaining point,
/// a 40 pt right-aligned percentage, then the reset caption.
///
/// Three things here are the fix for a measured defect rather than a
/// transcription (`/tmp/parity/delta-list.md` #18, #19, #28, #29):
///
///  - The bar is the grid's `1fr` column, so it FILLS the row (170 pt on this
///    panel). The pre-v4 bar was a fixed 72 pt, which made a 98 % window and a
///    30 % one look like similar amounts of ink.
///  - The percentage sits in its own fixed 40 pt column, right-aligned, so two
///    windows' digits line up and every `resets …` starts at one x.
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

    /// The window's own state, or `nil` when there is nothing to state.
    ///
    /// `.unknown` is a token THIS build cannot name, not a missing reading: the
    /// panel has no copy of the server's near-limit threshold, so it may not
    /// invent a band for it. It draws the sheet's own `.bar i.neutral` grey.
    /// What it must never do is what the first v4 render did — fall through to
    /// the `unmeasured` violet and paint a 98 % window the same colour as an
    /// account nothing has ever measured.
    private var role: QuotaState? {
        switch tint {
        case .unmeasured: return nil
        case .state(let state): return state
        }
    }

    private var fillTint: Color {
        switch role {
        case .some(.near): return Tok.near
        case .some(.spent): return Tok.spent
        case .some(.ok): return Tok.ok
        case .some(.unknown): return Tok.mute
        case .none: return Tok.unmeasured
        }
    }

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
                    RoundedRectangle(cornerRadius: V4.barRadius)
                        .fill(fillTint)
                        .frame(width: max(V4.barMinWidth, proxy.size.width * fraction))
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
        .accessibilityLabel("\(label) window, \(QuotaFormat.percent(value)) used")
    }
}
