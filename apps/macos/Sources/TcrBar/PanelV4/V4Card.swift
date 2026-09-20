import SwiftUI

/// `.card` — the panel's one box: `rgba(255,255,255,.045)` fill, a 1 pt
/// `line` border, radius 8, padding 10 12.
///
/// The border is a full 1 pt, not the half-point hairline the pre-v4 panel drew.
/// Measured against the mockup: card border 1 pt at alpha .09 there, 0.5 pt at
/// alpha .24 here — half the width at nearly three times the alpha, which reads
/// as a drawn outline rather than the seam the sheet asks for.
/// ## One card type, optionally tinted
///
/// `accent` is `nil` everywhere but the knock card, and `nil` is today's card
/// byte for byte. A non-`nil` accent washes the card fill with it and borders
/// in it, at ``V4/accentWashAlpha`` and ``V4/accentLineAlpha``, the pair the
/// "what yes does" block already drew at.
///
/// A parameter rather than a second card type. A card that needs to look
/// different is how two card vocabularies start, and the second one is always
/// the one that stops matching the sheet.
struct V4Card<Content: View>: View {
    /// The hue this card wears, or `nil` for the plain card.
    ///
    /// Reserved for a card with a DEADLINE on it: the knock card, which is the
    /// only thing on the Peers tab that will be gone in ten minutes whether or
    /// not anybody looks. A colour spent on a card that is simply there
    /// forever stops meaning anything on the card that is not.
    var accent: Color?
    @ViewBuilder var content: () -> Content

    private var fill: Color { accent?.opacity(V4.accentWashAlpha) ?? .clear }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            content()
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(.vertical, V4.cardInsetV)
        .padding(.horizontal, V4.cardInsetH)
        .background(
            RoundedRectangle(cornerRadius: V4.cardRadius)
                .fill(Tok.cardFill)
                // The wash sits OVER the card fill rather than replacing it,
                // which is what `color-mix(in srgb, near 7%, card)` means: the
                // card is still the panel's card, warmed.
                .overlay(RoundedRectangle(cornerRadius: V4.cardRadius).fill(fill))
        )
        .overlay(
            RoundedRectangle(cornerRadius: V4.cardRadius)
                .strokeBorder(
                    accent?.opacity(V4.accentLineAlpha) ?? Tok.cardLine,
                    lineWidth: V4.panelBorderWidth)
        )
    }
}

/// `.row` — `display:flex; justify-content:space-between; align-items:center;
/// gap:8`, and NO vertical padding: the sheet gives `padding:4px 0` to
/// `.sess .row` alone. A card row's height is its line box (`font:15px/1.4` →
/// 21 pt), which is what ``V4/rowLineHeight`` supplies.
///
/// ## Whitespace collapses before a character does
///
/// `leading()` takes `layoutPriority(1)` and the `Spacer` takes `minLength: 0`.
/// Without both, the identifier — the thing the row is FOR — is what gives way:
/// SwiftUI offers the stack's width around equally, the name truncates, and the
/// spacer sits at its minimum holding empty points the name needed. Measured on
/// the shipped panel: 31.5 pt of empty row against a `V4.rowGap` of 8 while the
/// address read "dave@exam…".
///
/// A `Spacer` is a view, so the stack's own `spacing` is charged on BOTH sides
/// of it — `minLength: V4.rowGap` made the real gap `rowGap * 3`. With
/// `minLength: 0` the stack spacing is the single source of that gap, which is
/// what `gap:8` means in the sheet.
///
/// Six sites in the Sessions tab already set the priority; no site under
/// `PanelV4/` did.
struct V4Row<Leading: View, Trailing: View>: View {
    @ViewBuilder var leading: () -> Leading
    @ViewBuilder var trailing: () -> Trailing

    var body: some View {
        HStack(alignment: .center, spacing: V4.rowGap) {
            // The label is BOUNDED and CLIPPED, not merely truncated.
            //
            // `MonoText` carries `.textSelection(.enabled)` so a command can
            // be copied (`V4Text.swift`), and a selectable `Text` lays its
            // FULL string out when it is clicked — a truncated command
            // re-drew itself at full length, under the pill, the moment the
            // operator clicked it (2026-09-13). `lineLimit` does not bind
            // that: a frame does.
            leading()
                .frame(maxWidth: .infinity, alignment: .leading)
                .clipped()
            Spacer(minLength: 0)
            // The PRIORITY sits on the trailing column, not on the label.
            //
            // It used to sit on `leading()`, which meant SwiftUI handed the
            // label its ideal width first: a command longer than the row
            // pushed the `Spacer` to zero and drew the `.fixedSize()` pill
            // ON TOP of its own last characters (seen in the Tools tab's
            // SLOWEST TODAY rows, 2026-09-13). The trailing column is the one
            // with a fixed width and no way to shrink; the leading label is
            // `MonoText`/`DimText`, which already carry `lineLimit(1)` and a
            // truncation mode and can give ground. So the pill claims its
            // width and the label ellipsises into what is left, which is what
            // every row here documents itself as doing.
            trailing()
                .fixedSize()
                .layoutPriority(1)
        }
        .frame(minHeight: V4.rowLineHeight)
    }
}

extension V4Row where Trailing == EmptyView {
    init(@ViewBuilder leading: @escaping () -> Leading) {
        self.init(leading: leading, trailing: { EmptyView() })
    }
}
