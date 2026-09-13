import SwiftUI
import TcrBarCore

/// `.panel` — the panel itself: 372 pt wide, radius 18, padding 10 10 8, a 1 pt
/// `line` border with a brighter top edge, and tabular figures throughout.
///
/// This is the shell every tab draws inside. It owns the chrome and nothing else:
/// the header, the summary block, the segmented control, and the footer's rule.
/// What goes between the strip and the footer is the tab's own business, which is
/// why `content` is a closure rather than a switch in here — the Accounts tab is
/// transcribed from the mockup under `PanelV4/`, while Sessions and Tools keep
/// the views they already had (Gil, 2026-09-13: "i think just accounts need
/// fixing … the others i think ours look better").
///
/// The corner, the border and the top edge are the shared-chrome fix. Measured
/// against the approved mockup: its corner rounds over 18 pt, row y=0 is the
/// `rgba(255,255,255,.18)` top edge and a 1 pt `line` border runs the perimeter,
/// while every earlier round of this panel had pixel (0,0) already at panel fill
/// — radius 0, no border, no top edge. It is the single largest block of wrong
/// pixels on all three tabs, and it is drawn once, here, for all of them.
struct PanelV4<Summary: View, Content: View, Footer: View>: View {
    var title: String = "tcr fleet"
    /// "updated 4s ago", or `nil` before the first poll lands.
    let freshness: String?
    let tabs: [PanelTab]
    let selected: PanelTab
    let badges: [PanelTab: Int]
    let onSelect: (PanelTab) -> Void
    let onSettings: () -> Void
    @ViewBuilder var summary: () -> Summary
    @ViewBuilder var content: () -> Content
    @ViewBuilder var footer: () -> Footer

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            PanelHeader(title: title, freshness: freshness, onSettings: onSettings)
            summary()
            SegmentedTabs(
                tabs: tabs, selected: selected, badges: badges, onSelect: onSelect)
            content()
            footer()
        }
        .padding(.top, V4.panelPaddingTop)
        .padding(.horizontal, V4.panelPaddingSide)
        .padding(.bottom, V4.panelPaddingBottom)
        .frame(width: V4.panelWidth)
        .background(
            RoundedRectangle(cornerRadius: V4.panelRadius, style: .continuous)
                .fill(Tok.panel)
        )
        .overlay(
            RoundedRectangle(cornerRadius: V4.panelRadius, style: .continuous)
                .strokeBorder(Tok.cardLine, lineWidth: V4.panelBorderWidth)
        )
        .overlay(alignment: .top) {
            // `border-top-color:rgba(255,255,255,.18)` — the lit edge that makes
            // the panel read as a raised surface rather than a painted rectangle.
            // Drawn as a clipped top segment rather than a second full border, so
            // the sides keep the `line` colour the sheet gives them.
            RoundedRectangle(cornerRadius: V4.panelRadius, style: .continuous)
                .strokeBorder(
                    Tok.ink.opacity(V4.panelTopEdgeAlpha), lineWidth: V4.panelBorderWidth
                )
                .mask(
                    LinearGradient(
                        colors: [.black, .clear],
                        startPoint: .top, endPoint: .bottom)
                )
                .allowsHitTesting(false)
        }
        .clipShape(RoundedRectangle(cornerRadius: V4.panelRadius, style: .continuous))
    }
}
