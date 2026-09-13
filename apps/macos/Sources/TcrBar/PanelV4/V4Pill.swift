import SwiftUI

/// `.pill` — an OUTLINED badge: no fill at all, a 1 pt border, and the label in
/// the role colour. 10.5 pt / 700 / +0.06em, uppercase, radius 7, padding 2 7.
///
/// The pre-v4 panel filled its pills and drew no border, which is the opposite
/// construction (`/tmp/parity/delta-list.md` #20) and reads as a chip rather than
/// a state. The role colours come from the gated palette, never from a group's
/// identity colour — the sheet is explicit that group colour is "outline and
/// legend only, never status".
struct V4Pill: View {
    enum Role {
        case neutral
        case ok
        case warn
        case bad
        case info

        var tint: Color {
            switch self {
            case .neutral: return Tok.dim
            case .ok: return Tok.ok
            case .warn: return Tok.near
            case .bad: return Tok.spent
            case .info: return Tok.unmeasured
            }
        }

        /// `.pill` alone borders in `line`; every role borders in its own colour.
        var border: Color {
            switch self {
            case .neutral: return Tok.cardLine
            case .ok, .warn, .bad, .info: return tint.opacity(V4.pillBorderAlpha)
            }
        }
    }

    let text: String
    var role: Role = .neutral

    var body: some View {
        Text(text.uppercased())
            .font(V4.font(V4.pillFontSize, .bold))
            .tracking(V4.pillTracking)
            .foregroundStyle(role.tint)
            .lineLimit(1)
            .fixedSize()
            .padding(.vertical, V4.pillPaddingV)
            .padding(.horizontal, V4.pillPaddingH)
            .overlay(
                RoundedRectangle(cornerRadius: V4.pillRadius)
                    .strokeBorder(role.border, lineWidth: V4.pillBorderWidth)
            )
            .accessibilityLabel(text)
    }
}
