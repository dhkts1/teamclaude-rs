import SwiftUI

/// `.seg` / `.tab` / `.badge` — a real tablist of real buttons.
///
/// Three measured fixes over the pre-v4 strip. The selected item carries
/// `rgba(255,255,255,.12)` and a shadow, where the old one washed .045 over its
/// container — +10 per channel against the mockup's +26, faint enough to read as
/// unselected. The icons draw in a 14 pt box instead of the 23–34 pt of glyph
/// they had grown to. And the badges render from the same counts on EVERY tab,
/// including the one being looked at: the old strip drew zero badge pixels on
/// Accounts while showing both counts on the other two.
struct SegmentedTabs: View {
    let tabs: [PanelTab]
    let selected: PanelTab
    /// A count per tab, or no entry at all. A zero never draws a badge — the
    /// panel's standing rule for counts.
    let badges: [PanelTab: Int]
    let onSelect: (PanelTab) -> Void

    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    var body: some View {
        HStack(spacing: V4.segGap) {
            ForEach(tabs, id: \.self) { tab in
                item(tab)
            }
        }
        .padding(V4.segPadding)
        .background(
            RoundedRectangle(cornerRadius: V4.segRadius).fill(Tok.ink.opacity(V4.segFillAlpha))
        )
        .padding(.bottom, V4.segMarginBottom)
        .accessibilityElement(children: .contain)
        .accessibilityLabel("TcrBar panel")
    }

    private func item(_ tab: PanelTab) -> some View {
        let isOn = tab == selected
        return Button {
            if reduceMotion {
                onSelect(tab)
            } else {
                withAnimation(
                    .spring(response: V4.springResponse, dampingFraction: V4.springDamping)
                ) {
                    onSelect(tab)
                }
            }
        } label: {
            HStack(spacing: V4.tabGap) {
                Image(systemName: tab.systemImage)
                    .font(.system(size: V4.tabLabelSize))
                    .frame(width: V4.tabIconBox, height: V4.tabIconBox)
                Text(tab.title)
                    .font(V4.font(V4.tabLabelSize, .semibold))
                    .tracking(V4.tabLabelTracking)
                if let count = badges[tab], count > 0 {
                    Text("\(count)")
                        .font(V4.font(V4.badgeSize))
                        .foregroundStyle(Tok.ink)
                        .contentTransition(.numericText())
                        .padding(.horizontal, V4.badgePaddingH)
                        .background(
                            RoundedRectangle(cornerRadius: V4.badgeRadius)
                                .fill(Tok.ink.opacity(V4.badgeFillAlpha))
                        )
                }
            }
            .foregroundStyle(isOn ? Tok.ink : Tok.dim)
            .frame(maxWidth: .infinity)
            .frame(minHeight: V4.tabMinHeight)
            .background(
                RoundedRectangle(cornerRadius: V4.tabRadius)
                    .fill(isOn ? Tok.ink.opacity(V4.tabSelectedAlpha) : Color.clear)
                    .shadow(
                        color: isOn ? Color.black.opacity(0.4) : .clear,
                        radius: 1, x: 0, y: 1)
            )
            .contentShape(Rectangle())
        }
        .buttonStyle(V4PressStyle())
        .accessibilityLabel(badges[tab].map { "\(tab.title), \($0)" } ?? tab.title)
        .accessibilityAddTraits(isOn ? [.isSelected] : [])
    }
}
