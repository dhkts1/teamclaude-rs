import SwiftUI
import TcrBarCore

/// `.grp` — a group of accounts inside a 1.5 pt outline in the group's own
/// identity colour, with its legend sitting ON the stroke.
///
/// The notch under the legend is CUT OUT of the stroke with a mask
/// (`.grp::before`'s two mask layers), never covered with an opaque patch: the
/// panel is translucent, and a patch would paint a rectangle of the wrong colour
/// over whatever is behind it. The mask is built from the legend's own measured
/// width, so a long group name cannot leave stroke showing through its text.
///
/// ## The identity colour draws the box, never the words
///
/// `color` reaches the 1.5 pt stroke and the 12 pt legend swatch and stops
/// there. The legend TEXT is always ``Tok/mute`` — the gated tertiary ink this
/// panel already gives every section head (``SectionHead``), 5.8:1 dark and
/// 4.8:1 light.
///
/// One value used to drive all three, which made the legend fail as text twice
/// over. With no identity hue the caller passed ``Tok/cardLine``, a 0.5 pt
/// DIVIDER colour: measured, `#c5c7cb` on the light panel is 1.52:1 and
/// `#393e43` on the dark is 1.68:1, both under APCA's discernible floor. With
/// one, the raw wire hue shipped into both appearances unchanged — `#92d188`
/// reads 1.62:1 light, `#c79ae8` 2.05:1. The legend is the only string that
/// names the box and counts it, so it is the one part of a group that may not
/// be drawn in a colour nobody gated.
struct GroupBox<Content: View>: View {
    let legend: String
    /// The group's identity hue, or `nil` for a group that has none. Drives the
    /// stroke and the legend's swatch; ``Tok/cardLine`` is the fallback for
    /// both, which is what that token is for.
    let color: Color?
    /// `.grp.collapsed{padding-bottom:8px}` — a box holding one line and a
    /// button closes a little further under it than one holding cards.
    var collapsed: Bool = false
    @ViewBuilder var content: () -> Content

    /// The legend's measured size. The width is where the notch ends; the
    /// height is what the legend is lifted by half of, so it sits CENTRED on the
    /// stroke rather than at a constant offset that only suits one font.
    @State private var legendSize: CGSize = .zero

    private var legendWidth: CGFloat { legendSize.width }

    private var notchWidth: CGFloat {
        V4.legendNotchWidth(forLegendWidth: legendWidth)
    }

    /// The stroke and the swatch: the group's hue, or the panel's own line
    /// colour when it has none.
    private var identityColor: Color { color ?? Tok.cardLine }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            content()
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(.top, V4.groupPaddingTop)
        .padding(.horizontal, V4.groupPaddingSide)
        .padding(.bottom, collapsed ? V4.groupPaddingBottomCollapsed : V4.groupPaddingBottom)
        .overlay(outline)
        .overlay(alignment: .topLeading) { legendLabel }
        .padding(.top, V4.groupMarginTop)
        .padding(.bottom, V4.groupMarginBottom)
        .accessibilityElement(children: .contain)
        .accessibilityLabel(legend)
    }

    private var outline: some View {
        RoundedRectangle(cornerRadius: V4.groupRadius)
            .strokeBorder(identityColor, lineWidth: V4.groupStroke)
            .mask(
                // Two bands, exactly as the CSS mask is: everything below the
                // legend's 9 pt band, plus that band with the legend's own span
                // punched out of it.
                VStack(spacing: 0) {
                    HStack(spacing: 0) {
                        Color.black.frame(width: V4.legendNotchStart)
                        Color.clear.frame(width: notchWidth)
                        Color.black
                    }
                    .frame(height: V4.legendMaskBand)
                    Color.black
                }
            )
            .allowsHitTesting(false)
    }

    private var legendLabel: some View {
        HStack(spacing: V4.legendGap) {
            RoundedRectangle(cornerRadius: V4.legendGlyph / 4)
                .stroke(identityColor, lineWidth: V4.panelBorderWidth)
                .frame(width: V4.legendGlyph, height: V4.legendGlyph)
            Text(legend)
                .font(V4.font(V4.legendFontSize, .bold))
                .tracking(V4.legendTracking)
                .foregroundStyle(Tok.mute)
                .lineLimit(1)
                .fixedSize()
        }
        .background(
            GeometryReader { proxy in
                Color.clear.preference(key: LegendSizeKey.self, value: proxy.size)
            }
        )
        .onPreferenceChange(LegendSizeKey.self) { legendSize = $0 }
        .padding(.leading, V4.legendNotchStart + V4.legendNotchPadding)
        .offset(y: V4.legendLift(forLegendHeight: legendSize.height))
        .accessibilityHidden(true)
    }
}

/// How big the legend actually drew: the notch matches its width and the lift
/// is half its height, so neither is a guessed constant.
struct LegendSizeKey: PreferenceKey {
    static var defaultValue: CGSize = .zero
    static func reduce(value: inout CGSize, nextValue: () -> CGSize) {
        let next = nextValue()
        value = CGSize(
            width: max(value.width, next.width), height: max(value.height, next.height))
    }
}
