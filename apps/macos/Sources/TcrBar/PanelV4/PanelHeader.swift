import SwiftUI

/// `.hdr` — the title and, on the same baseline, how fresh the reading is and
/// the gear that opens Settings.
///
/// The gear is a real 26 pt button with a fill and a radius, not a bare glyph.
/// Measured: the mockup draws a 26×26 pt box, radius 7, fill white .08, with a
/// 15 pt icon; the pre-v4 header drew 14×14 pt of glyph and no box at all, which
/// read as decoration and gave a pointer nothing to aim at.
struct PanelHeader: View {
    let title: String
    /// "updated 4s ago". `nil` before the first poll returns, when there is no
    /// freshness to claim.
    let freshness: String?
    let onSettings: () -> Void

    var body: some View {
        // Two nestings, one per alignment the row needs. `.hdr` is
        // `align-items:baseline` for its TEXT — the title and the freshness sit
        // on one baseline, which is what stops a 17 pt word and a 12.5 pt one
        // from looking like two rows — while the gear is a BOX and centres on
        // the line like any inline-block. One flat `.firstTextBaseline` stack
        // aligned all three by text metrics and lifted the gear 4 pt off the
        // row; the `alignmentGuide` that compensated for it was a constant
        // tuned to one font size.
        HStack(alignment: .center, spacing: V4.headerGap) {
            HStack(alignment: .firstTextBaseline, spacing: V4.headerGap) {
                Text(title)
                    .font(V4.font(V4.titleSize, .bold))
                    .tracking(V4.titleTracking)
                    .foregroundStyle(Tok.ink)
                    .layoutPriority(1)
                Spacer(minLength: 0)
                if let freshness {
                    Text(freshness)
                        .font(V4.font(V4.freshnessSize))
                        .foregroundStyle(Tok.mute)
                        .lineLimit(1)
                        .fixedSize()
                }
            }
            .frame(minHeight: V4.lineHeight(V4.titleSize))
            gear
        }
        .padding(.top, V4.headerPaddingTop)
        .padding(.horizontal, V4.headerPaddingSide)
        .padding(.bottom, V4.headerPaddingBottom)
    }

    private var gear: some View {
        Button(action: onSettings) {
            Image(systemName: "gearshape")
                .font(.system(size: V4.gearGlyphSize))
                .foregroundStyle(Tok.dim)
                .frame(width: V4.gearSize, height: V4.gearSize)
                .background(
                    RoundedRectangle(cornerRadius: V4.gearRadius)
                        .fill(Tok.ink.opacity(V4.gearFillAlpha))
                )
        }
        .buttonStyle(V4PressStyle(cornerRadius: V4.gearRadius))
        .accessibilityLabel("Settings")
        .keyboardShortcut(",", modifiers: .command)
        .help("Settings… ⌘,")
    }
}
