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
///  - The fill is a STATUS colour chosen from the reading (`ok` / `warn` at the
///    near threshold / `bad` over it) and never the enclosing group's identity
///    colour. A 98 % window used to draw in its group's violet beside an OK pill.
struct QuotaRow: View {
    let label: String
    /// `nil` is an unmeasured window: an explicit empty track, never a zero-width
    /// fill, because a zero reading and an absent one mean opposite things.
    let value: Double?
    let state: QuotaState?
    let resetAtMs: Int64?
    let now: Date

    private var fraction: Double {
        switch QuotaFormat.barFill(value) {
        case .measured(let v): return v
        case .unmeasured: return 0
        }
    }

    /// The window's own state when the server sent one, else the reading's own
    /// band. Never the account's overall `quotaState`: a healthy account can hold
    /// one spent window, which is the row this bar exists to show.
    private var role: QuotaState? {
        if let state { return state }
        guard let value else { return nil }
        if value >= 0.95 { return .spent }
        if value >= 0.80 { return .near }
        return .ok
    }

    private var fillTint: Color {
        switch role {
        case .some(.near): return Tok.near
        case .some(.spent): return Tok.spent
        case .some(.ok): return Tok.ok
        case .some(.unknown), .none: return Tok.unmeasured
        }
    }

    /// `.resets.warn` — the caption turns amber on the same threshold the bar
    /// does, so the row does not say "nearly out" in one place and stay neutral
    /// in the other.
    private var captionTint: Color {
        switch role {
        case .some(.near), .some(.spent): return Tok.near
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
        .padding(.top, V4.quotaMarginTop)
        .accessibilityElement(children: .combine)
        .accessibilityLabel("\(label) window, \(QuotaFormat.percent(value)) used")
    }
}
