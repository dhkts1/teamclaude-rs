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
    @ViewBuilder var content: () -> Content

    /// The legend's measured width, which is where the notch ends. Starts at the
    /// sheet's own `--n1` default so the first frame is close, then corrects.
    @State private var legendWidth: CGFloat = 0

    private var notchEnd: CGFloat {
        V4.legendNotchStart + V4.legendNotchPadding * 2 + legendWidth
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            content()
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(.top, V4.groupPaddingTop)
        .padding(.horizontal, V4.groupPaddingSide)
        .padding(.bottom, V4.groupPaddingBottom)
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
                        Color.clear.frame(width: max(0, notchEnd - V4.legendNotchStart))
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
                Color.clear.preference(key: LegendWidthKey.self, value: proxy.size.width)
            }
        )
        .onPreferenceChange(LegendWidthKey.self) { legendWidth = $0 }
        .padding(.leading, V4.legendNotchStart + V4.legendNotchPadding)
        .offset(y: V4.legendOffsetY)
        .accessibilityHidden(true)
    }
}

/// How wide the legend actually drew, so the stroke's notch matches it rather
/// than a guessed constant.
struct LegendWidthKey: PreferenceKey {
    static var defaultValue: CGFloat = 0
    static func reduce(value: inout CGFloat, nextValue: () -> CGFloat) {
        value = max(value, nextValue())
    }
}
