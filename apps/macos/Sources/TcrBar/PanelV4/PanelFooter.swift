import SwiftUI

/// `.foot` — the provenance line: a 1 pt top rule, 10 pt above it, 8 pt below,
/// and 12.5 pt `mute` text with one fact on each end.
///
/// Per TAB, not global (`/tmp/parity/delta-list.md` #16): the mockup gives the
/// Sessions panel "proxy + ~/.claude/sessions" and the Tools panel "from request
/// bodies only · nothing logged", because where a number came from is the one
/// thing a footer can say that the rows above it cannot.
struct PanelFooter<Extras: View>: View {
    let leading: String
    var leadingSystemImage: String?
    var trailing: String?
    /// Anything that must sit ABOVE the rule — the action row, a failure line,
    /// the not-supervised controls. The sheet puts the buttons above the rule and
    /// the text below it, which is the order the pre-v4 panel had inverted.
    @ViewBuilder var extras: () -> Extras

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            extras()
            Rectangle()
                .fill(Tok.cardLine)
                .frame(height: V4.footerRuleWidth)
                .padding(.top, V4.footerMarginTop)
            HStack(spacing: V4.footerGap) {
                HStack(spacing: V4.sectionHeadGap) {
                    if let leadingSystemImage {
                        Image(systemName: leadingSystemImage)
                            .font(.system(size: V4.footerGlyph))
                            .accessibilityHidden(true)
                    }
                    Text(leading)
                        .lineLimit(1)
                }
                Spacer(minLength: V4.footerGap)
                if let trailing {
                    Text(trailing).lineLimit(1)
                }
            }
            .font(V4.font(V4.footerSize))
            .foregroundStyle(Tok.mute)
            .padding(.top, V4.footerPaddingTop)
        }
    }
}

extension PanelFooter where Extras == EmptyView {
    init(leading: String, leadingSystemImage: String? = nil, trailing: String? = nil) {
        self.init(
            leading: leading, leadingSystemImage: leadingSystemImage, trailing: trailing,
            extras: { EmptyView() })
    }
}
