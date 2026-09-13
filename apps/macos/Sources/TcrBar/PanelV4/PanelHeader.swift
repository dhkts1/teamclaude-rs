import SwiftUI

/// `.hdr` — the title and, on the same baseline, how fresh the reading is and
/// the gear that opens Settings.
///
/// The gear is a real 26 pt button with a fill and a radius, not a bare glyph
/// (`/tmp/parity/delta-list.md` #3): at 14 pt of unfilled icon it read as
/// decoration and gave a pointer nothing to aim at.
struct PanelHeader: View {
    let title: String
    /// "updated 4s ago". `nil` before the first poll returns, when there is no
    /// freshness to claim.
    let freshness: String?
    let onSettings: () -> Void

    var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: V4.headerGap) {
            Text(title)
                .font(V4.font(V4.titleSize, .bold))
                .tracking(V4.titleTracking)
                .foregroundStyle(Tok.ink)
            Spacer(minLength: V4.headerGap)
            if let freshness {
                Text(freshness)
                    .font(V4.font(V4.freshnessSize))
                    .foregroundStyle(Tok.mute)
                    .lineLimit(1)
            }
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
        .buttonStyle(V4PressStyle())
        .accessibilityLabel("Settings")
        .keyboardShortcut(",", modifiers: .command)
        .help("Settings… ⌘,")
        // The gear sits on the title's baseline row, not on its text baseline:
        // it is a box, and aligning a box by the baseline of the label beside it
        // is what put it 4 pt low on every earlier round.
        .alignmentGuide(.firstTextBaseline) { $0[.bottom] - V4.headerPaddingBottom / 2 }
    }
}
