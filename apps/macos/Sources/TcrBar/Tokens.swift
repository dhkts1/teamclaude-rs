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
    /// Enabled, but nothing has ever been measured about it.
    ///
    /// Its own hue for the same reason `FleetTally.Kind.unmeasured` is its own
    /// bucket: `ok` would overclaim capacity, `spent` would claim an exhaustion
    /// nobody observed, and `unknown` means "a state this build cannot name",
    /// which is a different fact from "no state at all". Deliberately
    /// low-chroma — an absent reading is not an alarm, so it must not sit on
    /// the traffic-light scale at all.
    public static let unmeasured = Color(nsColor: .systemGray)
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
    public static let rowSpacing: CGFloat = 6
    public static let tightSpacing: CGFloat = 4
    public static let barHeight: CGFloat = 5
    public static let barRadius: CGFloat = 3
    public static let pillRadius: CGFloat = 4
    public static let pillPaddingH: CGFloat = 5
    public static let pillPaddingV: CGFloat = 1

    // MARK: Row density
    //
    // The fleet is 13 accounts, and every one of them is on screen at once. At
    // the original spacing the panel read as a wall, so the *secondary* text and
    // the padding shrink while the account name — the one thing being scanned
    // for — keeps its size. Density lives here as named metrics so the view has
    // no magic numbers to drift out of sync.

    /// Vertical breathing room around one account row.
    public static let rowPaddingV: CGFloat = 2
    /// Gap between the lines *inside* one account row. Tighter than the gap
    /// between rows, so rows still read as separate blocks.
    public static let rowLineSpacing: CGFloat = 2
    /// Secondary text: percentages, status, the reset countdown.
    public static let secondaryFontSize: CGFloat = 10
    /// Tertiary text: counters, error detail, the pills.
    public static let detailFontSize: CGFloat = 9

    // MARK: Row fonts
    //
    // Explicit point sizes rather than `.caption`/`.caption2`, because the whole
    // point is to pick sizes that fit the fleet. `Font.system(size:)` on macOS
    // still honours the Accessibility text-size setting, so this costs no
    // adaptivity.

    public static var secondaryFont: Font { .system(size: secondaryFontSize) }
    public static var secondaryDigitFont: Font { secondaryFont.monospacedDigit() }
    public static var detailFont: Font { .system(size: detailFontSize) }
    public static var detailDigitFont: Font { detailFont.monospacedDigit() }
    public static var pillFont: Font { .system(size: detailFontSize, weight: .semibold) }

    // MARK: Mapping

    public static func color(for state: QuotaState) -> Color {
        switch state {
        case .ok: return ok
        case .near: return near
        case .spent: return spent
        case .unknown: return unknown
        }
    }

    public static func color(for kind: FleetTally.Kind) -> Color {
        switch kind {
        case .ok: return ok
        case .near: return near
        case .spent: return spent
        case .unknown: return unknown
        case .unmeasured: return unmeasured
        case .disabled: return disabled
        }
    }

    /// Menu-bar glyph for the fleet's capacity state (`Fleet.capacityGlyphState`).
    /// SF Symbols only, so it renders as a template image and follows the menu
    /// bar's own tint.
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
