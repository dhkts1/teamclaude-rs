import SwiftUI

/// `.card` — the panel's one box: `rgba(255,255,255,.045)` fill, a 1 pt
/// `line` border, radius 8, padding 10 12.
///
/// The border is a full 1 pt, not the half-point hairline the pre-v4 panel drew:
/// measured against the mockup, the old one was half the width at nearly three
/// times the alpha (`/tmp/parity/delta-list.md` #11), which reads as a drawn
/// outline rather than the seam the sheet asks for.
struct V4Card<Content: View>: View {
    @ViewBuilder var content: () -> Content

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            content()
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(.vertical, V4.cardPaddingV)
        .padding(.horizontal, V4.cardPaddingH)
        .background(RoundedRectangle(cornerRadius: V4.cardRadius).fill(Tok.cardFill))
        .overlay(
            RoundedRectangle(cornerRadius: V4.cardRadius)
                .strokeBorder(Tok.cardLine, lineWidth: V4.panelBorderWidth)
        )
    }
}

/// `.row` — `display:flex; justify-content:space-between; align-items:center;
/// gap:8` with `padding:4px 0`, so two adjacent rows sit 8 pt apart and the card's
/// own 10 pt padding is not doubled by the first and last row's.
struct V4Row<Leading: View, Trailing: View>: View {
    @ViewBuilder var leading: () -> Leading
    @ViewBuilder var trailing: () -> Trailing

    var body: some View {
        HStack(alignment: .center, spacing: V4.rowGap) {
            leading()
            Spacer(minLength: V4.rowGap)
            trailing()
        }
        .padding(.vertical, V4.rowPaddingV)
    }
}

extension V4Row where Trailing == EmptyView {
    init(@ViewBuilder leading: @escaping () -> Leading) {
        self.init(leading: leading, trailing: { EmptyView() })
    }
}
