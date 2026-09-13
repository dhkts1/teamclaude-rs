import SwiftUI

/// `.card` — the panel's one box: `rgba(255,255,255,.045)` fill, a 1 pt
/// `line` border, radius 8, padding 10 12.
///
/// The border is a full 1 pt, not the half-point hairline the pre-v4 panel drew.
/// Measured against the mockup: card border 1 pt at alpha .09 there, 0.5 pt at
/// alpha .24 here — half the width at nearly three times the alpha, which reads
/// as a drawn outline rather than the seam the sheet asks for.
struct V4Card<Content: View>: View {
    @ViewBuilder var content: () -> Content

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            content()
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(.vertical, V4.cardInsetV)
        .padding(.horizontal, V4.cardInsetH)
        .background(RoundedRectangle(cornerRadius: V4.cardRadius).fill(Tok.cardFill))
        .overlay(
            RoundedRectangle(cornerRadius: V4.cardRadius)
                .strokeBorder(Tok.cardLine, lineWidth: V4.panelBorderWidth)
        )
    }
}

/// `.row` — `display:flex; justify-content:space-between; align-items:center;
/// gap:8`, and NO vertical padding: the sheet gives `padding:4px 0` to
/// `.sess .row` alone. A card row's height is its line box (`font:15px/1.4` →
/// 21 pt), which is what ``V4/rowLineHeight`` supplies.
struct V4Row<Leading: View, Trailing: View>: View {
    @ViewBuilder var leading: () -> Leading
    @ViewBuilder var trailing: () -> Trailing

    var body: some View {
        HStack(alignment: .center, spacing: V4.rowGap) {
            leading()
            Spacer(minLength: V4.rowGap)
            trailing()
        }
        .frame(minHeight: V4.rowLineHeight)
    }
}

extension V4Row where Trailing == EmptyView {
    init(@ViewBuilder leading: @escaping () -> Leading) {
        self.init(leading: leading, trailing: { EmptyView() })
    }
}
