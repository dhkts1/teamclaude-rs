import SwiftUI
import TcrBarCore

/// The one place a colour or a metric is written down.
///
/// This is a status dashboard, not a brand surface, so the palette is
/// conventional traffic-light semantics over a neutral panel, and it leans on
/// system semantic colours wherever one fits — those adapt to light/dark,
/// Increased Contrast and the accessibility settings for free.
public enum Tok {
    // MARK: Status hues

    /// In rotation, comfortably under threshold.
    public static let ok = Color.green
    /// Held out of rotation but the credential is fine and headroom remains.
    public static let near = Color.orange
    /// Fully spent on a gating window until it resets.
    public static let spent = Color.red
    /// Operator-disabled: not an alarm, a decision. Deliberately muted.
    public static let disabled = Color.secondary
    /// A value we could not classify — visible, but never dressed as healthy.
    public static let unknown = Color.purple
    /// Numbers that are structurally zero rather than measured.
    public static let offline = Color(nsColor: .tertiaryLabelColor)

    // MARK: Surfaces

    public static let panel = Color(nsColor: .windowBackgroundColor)
    public static let track = Color(nsColor: .quaternaryLabelColor)
    public static let hairline = Color(nsColor: .separatorColor)
    public static let accent = Color(nsColor: .controlAccentColor)

    // MARK: Metrics

    public static let panelWidth: CGFloat = 380
    public static let panelMaxHeight: CGFloat = 520
    public static let gutter: CGFloat = 12
    public static let rowSpacing: CGFloat = 10
    public static let tightSpacing: CGFloat = 4
    public static let barHeight: CGFloat = 6
    public static let barRadius: CGFloat = 3
    public static let pillRadius: CGFloat = 4
    public static let pillPaddingH: CGFloat = 6
    public static let pillPaddingV: CGFloat = 2

    // MARK: Mapping

    public static func color(for state: QuotaState) -> Color {
        switch state {
        case .ok: return ok
        case .near: return near
        case .spent: return spent
        case .unknown: return unknown
        }
    }

    /// Menu-bar glyph for the worst account in the fleet. SF Symbols only, so it
    /// renders as a template image and follows the menu bar's own tint.
    public static func glyph(for state: QuotaState) -> String {
        switch state {
        case .ok: return "gauge.with.dots.needle.33percent"
        case .near: return "gauge.with.dots.needle.67percent"
        case .spent: return "gauge.with.dots.needle.100percent"
        case .unknown: return "questionmark.circle"
        }
    }

    /// Glyph for a state where there is nothing to gauge.
    public static let unreadableGlyph = "exclamationmark.triangle"
}
