import SwiftUI

/// `.ring` — 34 × 34, stroke 4, an `rgba(255,255,255,.08)` track, and `bad`
/// when what it measures is nearly up.
///
/// Every running call draws one (Gil, 2026-09-13, on his own Tools crop: the
/// row closest to the timeout was the ONLY row with no ring). The panel knows
/// exactly one denominator — the 600 s tool timeout — and the section head says
/// so out loud in its `.unit`, which is what makes a ring with a shared
/// denominator honest: the reader is told what the whole is before reading any
/// fraction of it. The earlier rule (a ring for `Bash` only, because the
/// timeout is the Bash tool's) left the reader to notice that an unringed row
/// meant "no denominator" rather than "nothing to worry about", and the row it
/// silently applied to was the one at 9m 40s.
struct ProgressRing: View {
    /// 0…1. Clamped here, not by the caller: an elapsed time can exceed its
    /// timeout by the width of one poll interval.
    let fraction: Double
    var tint: Color = Tok.ok
    /// What the ring is a fraction OF, for the reader who cannot see it.
    var accessibilityText: String?

    var body: some View {
        ZStack {
            Circle()
                .stroke(Tok.ink.opacity(V4.ringTrackAlpha), lineWidth: V4.ringStroke)
            Circle()
                .trim(from: 0, to: max(0, min(fraction, 1)))
                .stroke(tint, style: StrokeStyle(lineWidth: V4.ringStroke, lineCap: .round))
                .rotationEffect(.degrees(-90))
        }
        .frame(width: V4.ringSize, height: V4.ringSize)
        .accessibilityHidden(accessibilityText == nil)
        .accessibilityLabel(accessibilityText ?? "")
    }
}

/// The fixed column every row's trailing content sits in.
///
/// One width for every row on a tab, so the ring in one row, the sparkline in
/// the next and the duration in the third all begin at the same x. Without it
/// each row sized its own trailing stack from its own content and the column
/// zig-zagged down the tab — which is what a reader scanning for "which of
/// these is slowest" has to fight.
struct TrailingColumn<Content: View>: View {
    var width: CGFloat = V4.trailingColumnWidth
    var alignment: HorizontalAlignment = .trailing
    @ViewBuilder var content: () -> Content

    var body: some View {
        content()
            .frame(width: width, alignment: alignment == .leading ? .leading : .trailing)
    }
}
