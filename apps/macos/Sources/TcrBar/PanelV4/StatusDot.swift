import SwiftUI
import TcrBarCore

/// `.dot` — an 8 pt disc with a 6 pt trailing gap, and for `busy`/`waiting` a
/// 3 pt halo at .18 alpha of its own colour (`box-shadow:0 0 0 3px`).
///
/// Never the only carrier of its meaning: every row that draws one also writes
/// the state in words beside it ("waiting 12m", "idle 40m"), so the panel still
/// reads without colour.
struct StatusDot: View {
    let activity: SessionActivity

    /// The idle dot takes no halo — the sheet gives one only to `busy` and
    /// `wait`, which is what makes a live session findable in a column of them.
    private var hasHalo: Bool { activity == .busy || activity == .waiting }

    private var tint: Color {
        switch activity {
        case .busy: return Tok.statusBusy
        case .waiting: return Tok.statusWaiting
        case .idle, .unknown: return Tok.statusIdle
        }
    }

    var body: some View {
        Circle()
            .fill(tint)
            .frame(width: V4.dotSize, height: V4.dotSize)
            .background(
                Circle()
                    .fill(hasHalo ? Tok.halo(tint) : Color.clear)
                    .frame(width: V4.dotHaloSize, height: V4.dotHaloSize)
            )
            .accessibilityHidden(true)
    }
}

/// The summary line's dots, which stand for a COUNT of sessions in a state
/// rather than one session — same geometry, no activity of their own.
struct SummaryDot: View {
    let tint: Color
    var halo: Bool = true

    var body: some View {
        Circle()
            .fill(tint)
            .frame(width: V4.dotSize, height: V4.dotSize)
            .background(
                Circle()
                    .fill(halo ? Tok.halo(tint) : Color.clear)
                    .frame(width: V4.dotHaloSize, height: V4.dotHaloSize)
            )
            .accessibilityHidden(true)
    }
}
