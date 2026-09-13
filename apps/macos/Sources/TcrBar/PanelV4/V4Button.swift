import SwiftUI

/// `.btn` — a real control: 13 pt label on `rgba(255,255,255,.10)`, a `line`
/// border, radius 7, min-height 28, padding 5 11.
struct V4Button: View {
    let title: String
    var help: String?
    let action: () -> Void

    var body: some View {
        Button(action: action) {
            Text(title)
                .font(V4.font(V4.buttonFontSize))
                .foregroundStyle(Tok.ink)
                .lineLimit(1)
                .padding(.vertical, V4.buttonPaddingV)
                .padding(.horizontal, V4.buttonPaddingH)
                .frame(minHeight: V4.buttonMinHeight)
                .background(
                    RoundedRectangle(cornerRadius: V4.buttonRadius)
                        .fill(Tok.ink.opacity(V4.buttonFillAlpha))
                )
                .overlay(
                    RoundedRectangle(cornerRadius: V4.buttonRadius)
                        .strokeBorder(Tok.cardLine, lineWidth: V4.panelBorderWidth)
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
            .frame(maxWidth: .infinity)
            .padding(.vertical, V4.discPaddingV)
            .padding(.horizontal, V4.discPaddingH)
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
