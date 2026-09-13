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
        .buttonStyle(V4PressStyle(cornerRadius: V4.buttonRadius))
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
        .buttonStyle(V4PressStyle(cornerRadius: V4.discRadius))
        .padding(.top, V4.discMarginTop)
        .accessibilityLabel(title)
        .accessibilityAddTraits(expanded ? .isSelected : [])
        .help(help ?? title)
    }
}

/// The sheet's press gesture and its hover, for every control on the panel.
///
/// Press: `scale .96 over 120 ms`, and under Reduce Motion an opacity dip
/// instead — `@media (prefers-reduced-motion:reduce)` replaces the scale with
/// exactly that.
///
/// Hover: `background:rgba(255,255,255,.07)` over 150 ms. `rg onHover` across
/// the whole target returned ZERO hits before this, so nothing on the v4 panel
/// answered the pointer at all — a button, a disclosure and a tab were
/// indistinguishable from the card behind them until clicked.
///
/// Applied in the STYLE rather than per control, because all four v4 controls
/// already share it (`V4Button`, `V4Disclosure`, `SegmentedTabs`,
/// `PanelHeader`'s gear) and a hover written four times is four things to keep
/// in step. It is ADDITIVE — drawn over whatever fill the control already has
/// — which is how one value reproduces the sheet's two: `.more` goes 0 to
/// `.07` and `.btn` `.10` to `.17`.
///
/// Reduce Motion keeps the hover and drops its TRANSITION, which is what the
/// sheet does: the `.15s` lives inside `@media (prefers-reduced-motion:
/// no-preference)`, and the state change itself is information, not motion.
/// Removing it would take the pointer feedback away from the people most
/// likely to need it.
struct V4PressStyle: ButtonStyle {
    /// The control's own corner radius, so the hover fill matches its shape
    /// rather than squaring off inside it. Defaults to the button/disclosure
    /// radius; the gear and the tabs pass their own.
    var cornerRadius: CGFloat = V4.buttonRadius

    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @State private var hovering = false

    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .background(
                RoundedRectangle(cornerRadius: cornerRadius)
                    .fill(Tok.ink.opacity(hovering ? V4.hoverFillAlpha : 0))
                    .animation(
                        reduceMotion ? nil : .easeOut(duration: V4.hoverDuration),
                        value: hovering)
            )
            .scaleEffect(
                configuration.isPressed && !reduceMotion ? V4.pressScale : 1,
                anchor: .center
            )
            .opacity(configuration.isPressed && reduceMotion ? 0.7 : 1)
            .animation(.easeOut(duration: 0.12), value: configuration.isPressed)
            .onHover { hovering = $0 }
    }
}
