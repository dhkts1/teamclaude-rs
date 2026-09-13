import SwiftUI

/// `.btn` — a real control: 13 pt label on `rgba(255,255,255,.10)`, a `line`
/// border, radius 7, min-height 28, padding 5 11.
///
/// `.btn.danger` is the sheet's own second variant — `color:var(--bad)` with a
/// `rgba(239,107,107,.38)` border. "Take over port…" is the only one the panel
/// draws, and it draws it in the action row like any other button: the pre-v4
/// panel gave it a hairline and a strip of its own BELOW the footer text, which
/// put the most expensive control on the panel in the position a reader scans
/// last. The mockup's order is buttons, then the rule, then the footer line; the
/// old panel had it exactly inverted.
struct V4Button: View {
    enum Role {
        case normal
        case danger

        var tint: Color {
            switch self {
            case .normal: return Tok.ink
            case .danger: return Tok.spent
            }
        }

        var border: Color {
            switch self {
            case .normal: return Tok.cardLine
            case .danger: return Tok.spent.opacity(V4.dangerBorderAlpha)
            }
        }
    }

    let title: String
    var role: Role = .normal
    var help: String?
    let action: () -> Void

    var body: some View {
        Button(action: action) {
            Text(title)
                .font(V4.font(V4.buttonFontSize))
                .foregroundStyle(role.tint)
                .lineLimit(1)
                .frame(minHeight: V4.lineHeight(V4.buttonFontSize))
                .padding(.vertical, V4.buttonInsetV)
                .padding(.horizontal, V4.buttonInsetH)
                .frame(minHeight: V4.buttonMinHeight)
                .background(
                    RoundedRectangle(cornerRadius: V4.buttonRadius)
                        .fill(Tok.ink.opacity(V4.buttonFillAlpha))
                )
                .overlay(
                    RoundedRectangle(cornerRadius: V4.buttonRadius)
                        .strokeBorder(role.border, lineWidth: V4.panelBorderWidth)
                )
        }
        .buttonStyle(V4PressStyle())
        .accessibilityLabel(title)
        .help(help ?? title)
    }
}

/// `.disc` / `.more` — the disclosure control under a capped list. Full width,
/// centred, 12.5 pt / 600, with a chevron that points the way it will move.
struct V4Disclosure: View {
    let title: String
    var expanded: Bool = false
    var help: String?
    let action: () -> Void

    var body: some View {
        Button(action: action) {
            HStack(spacing: V4.tabGap) {
                Image(systemName: expanded ? "chevron.up" : "chevron.down")
                    .font(.system(size: V4.discGlyph, weight: .semibold))
                Text(title)
                    .font(V4.font(V4.discFontSize, .semibold))
                    .lineLimit(1)
            }
            .foregroundStyle(Tok.dim)
            .frame(maxWidth: .infinity, minHeight: V4.lineHeight(V4.discFontSize))
            .padding(.vertical, V4.discInsetV)
            .padding(.horizontal, V4.discInsetH)
            .frame(minHeight: V4.buttonMinHeight)
            .overlay(
                RoundedRectangle(cornerRadius: V4.discRadius)
                    .strokeBorder(Tok.cardLine, lineWidth: V4.panelBorderWidth)
            )
        }
        .buttonStyle(V4PressStyle())
        .padding(.top, V4.discMarginTop)
        .accessibilityLabel(title)
        .accessibilityAddTraits(expanded ? .isSelected : [])
        .help(help ?? title)
    }
}

/// The sheet's one press gesture: `scale .96 over 120 ms`, and under Reduce
/// Motion an opacity dip instead — `@media (prefers-reduced-motion:reduce)`
/// replaces the scale with exactly that.
struct V4PressStyle: ButtonStyle {
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .scaleEffect(
                configuration.isPressed && !reduceMotion ? V4.pressScale : 1,
                anchor: .center
            )
            .opacity(configuration.isPressed && reduceMotion ? 0.7 : 1)
            .animation(.easeOut(duration: 0.12), value: configuration.isPressed)
    }
}
