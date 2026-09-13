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
struct GroupBox<Content: View>: View {
    let legend: String
    let color: Color
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
            .strokeBorder(color, lineWidth: V4.groupStroke)
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
                .stroke(color, lineWidth: V4.panelBorderWidth)
                .frame(width: V4.legendGlyph, height: V4.legendGlyph)
            Text(legend)
                .font(V4.font(V4.legendFontSize, .bold))
                .tracking(V4.legendTracking)
                .foregroundStyle(color)
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
