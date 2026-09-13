import AppKit
import SwiftUI
import TcrBarCore

/// The v4 number sheet, as Swift.
///
/// The mockup's `<style>` block, extracted here as one name per CSS declaration, at
/// 1 CSS px = 1 pt. It is the ONE file under `PanelV4/` allowed to hold a raw
/// number — `scripts/check-panel-v4.sh` fails the build if any sibling view
/// writes a literal size or reaches for a pre-v4 geometry token, because the
/// eleven adaptation rounds before this one drifted exactly that way: a 16 here,
/// an 8 there, each locally defensible and collectively a different design.
///
/// Colour is deliberately NOT restated here. `Tok` already carries the v4 sheet's
/// own role names (`Tok.cardFill`, `Tok.cardLine`, `Tok.dim`, `Tok.mute`) bound to
/// the gated palette, and a second set of hexes in this file would be a second
/// source of truth for the thing `scripts/tcrbar-palette.py` exists to gate. The
/// two exceptions below (`trend`, `info`) are roles the palette has no token for
/// and are named as such.
enum V4 {
    /// Whether the type and box tokens below shrink (Gil, 2026-09-13: "yes the
    /// right compact is better").
    ///
    /// ``PanelDensityPreference/resolved(defaults:accounts:)``, not
    /// `current() == .compact`: the shipped default is `.auto`, and against
    /// `.auto` that comparison is false — which would have quietly made
    /// Comfortable the default for every fleet, whatever its size. The
    /// resolver is the one place that decides, and it is read rather than
    /// stored: every token below is a computed `static var` re-evaluated on
    /// each draw, so the Settings picker AND a fleet that grew past the
    /// ceiling both take effect on the panel's next redraw with nothing to
    /// wire, restart or invalidate.
    static var compact: Bool { PanelDensityPreference.resolved() == .compact }

    // MARK: - Panel (`.panel`)

    /// The mockup's own `width:372px`. `Tok.panelWidth` is bound to this, so the
    /// popover, the size probe and this view cannot disagree about how wide the
    /// panel is.
    static let panelWidth: CGFloat = 372
    static let panelRadius: CGFloat = 18
    static let panelPaddingTop: CGFloat = 10
    static let panelPaddingSide: CGFloat = 10
    static let panelPaddingBottom: CGFloat = 8
    /// `border:1px solid var(--line)` with `border-top-color:rgba(255,255,255,.18)`.
    static let panelBorderWidth: CGFloat = 1
    static let panelTopEdgeAlpha: Double = 0.18

    // MARK: - Header (`.hdr`)

    static let headerPaddingTop: CGFloat = 2
    static let headerPaddingSide: CGFloat = 4
    static let headerPaddingBottom: CGFloat = 6
    /// `.gear` — a 26×26 button, radius 7, `rgba(255,255,255,.08)`, 15 pt glyph.
    static let gearSize: CGFloat = 26
    static let gearRadius: CGFloat = 7
    static let gearFillAlpha: Double = 0.08
    static let gearGlyphSize: CGFloat = 15
    /// `.hdr` puts the title and the freshness on one baseline with a gap.
    static let headerGap: CGFloat = 8

    // MARK: - Summary (`.sum`)

    static let summaryPaddingSide: CGFloat = 4
    static let summaryPaddingBottom: CGFloat = 10

    // MARK: - Segmented control (`.seg` / `.tab` / `.badge`)

    static let segRadius: CGFloat = 10
    static let segPadding: CGFloat = 3
    static let segGap: CGFloat = 3
    static let segFillAlpha: Double = 0.06
    static var segMarginBottom: CGFloat { compact ? 8 : 12 }
    static var tabMinHeight: CGFloat { compact ? 28 : 32 }
    static let tabRadius: CGFloat = 8
    static let tabSelectedAlpha: Double = 0.12
    static let tabIconBox: CGFloat = 14
    static let tabIconStroke: CGFloat = 1.8
    static let tabGap: CGFloat = 6
    static let badgeRadius: CGFloat = 9
    static let badgePaddingH: CGFloat = 6
    static let badgeFillAlpha: Double = 0.14

    // MARK: - Card (`.card` / `.row`)

    static let cardRadius: CGFloat = 8
    static var cardPaddingV: CGFloat { compact ? 7 : 10 }
    static var cardPaddingH: CGFloat { compact ? 10 : 12 }
    /// `.card{margin:14px 0}` — adjacent cards collapse to one 14 pt gap.
    static var cardGap: CGFloat { compact ? 8 : 14 }
    /// Padding PLUS the border, which is what `box-sizing:border-box` charges a
    /// CSS box for and what a SwiftUI `strokeBorder` overlay does not: the
    /// overlay takes no layout at all, so a card padded by the CSS figure alone
    /// draws 2 pt shorter and 2 pt narrower than the same card in the browser.
    /// Measured on the first v4 render: 41 pt against the mockup's 43.
    static var cardInsetV: CGFloat { cardPaddingV + panelBorderWidth }
    static var cardInsetH: CGFloat { cardPaddingH + panelBorderWidth }
    /// The flex gap inside a `.row`.
    static var rowGap: CGFloat { compact ? 6 : 8 }
    /// `.sess .row{padding:2px 0}` — the ONLY `.row` in the sheet with vertical
    /// padding, and it is the session block's. A card's rows have none: their
    /// height is their line box and nothing else, which is what
    /// ``lineHeight(_:)`` supplies. Measured: padding the card's rows as well
    /// made every account card 3 pt tall per row — a parked card measured 47 pt
    /// against the mockup's 43.
    ///
    /// Flat 2 in both densities — the mockup carries no density split here,
    /// and round 1's `3 : 4` was measured against an EARLIER copy that read
    /// 4px; the current one on disk reads 2px in both the busy-session row
    /// and its subline. Round 2 also wires this into ``sessionRow(_:)`` for
    /// the first time — it was declared and never applied, so "matching" the
    /// mockup's number had no visible effect until now.
    static let sessRowPaddingV: CGFloat = 2

    // MARK: - Quota grid (`.q`)

    /// `grid-template-columns:24px 1fr 40px auto` — with the first track
    /// widened from the sheet's 24.
    ///
    /// 24 pt fits `5h` and `7d`, which is every label the mockup's card has.
    /// The Fable weekly window's label is a WORD, and at 24 pt `fable` wrapped
    /// to `fabl` / `e` and grew the row by a line — measured in
    /// `01-healthy-auto-dark.png` before this changed. One track, not two:
    /// three bars starting at three different x is the misalignment this grid
    /// exists to prevent, so the column is sized for the longest label the card
    /// can draw and every row keeps it.
    static var quotaLabelWidth: CGFloat { compact ? 32 : 34 }
    static let quotaPercentWidth: CGFloat = 40
    static let quotaGap: CGFloat = 8

    /// The reset caption's own column — FIXED, and reserved on every
    /// `QuotaRow` whether or not that row draws a caption at all (Gil,
    /// 2026-09-13: "not aligned nicely" — the caption used to be
    /// `.fixedSize()`, an auto-width column, so the bar beside it was
    /// whatever was left over: 66pt on alice's 5h row (`"in 2h 10m"`) against
    /// 94.5pt on her 7d row (`"in 3d"`), and a row with no live caption at
    /// all got the whole remainder. One bar length per card, and one across
    /// every card, needs one column width regardless of content.
    ///
    /// Sized by MEASURING every shape ``QuotaFormat/resetCaption(resetAtMs:now:)``
    /// can print, at Comfortable's 12 pt, the same way ``usageTailWidth`` was:
    /// the day tier never carries minutes (`"6d 23h"`, `"9d 23h"` — the days
    /// digit does not change the width because `duration(minutes:)` never
    /// prints more of it than fits one digit's row here), so the WIDEST shape
    /// is actually the hour tier's own ceiling, `"in 23h 59m"` at 64.0pt —
    /// wider than the day-tier example that motivated this column
    /// (`"in 4d 12h"`, 51.7pt), because a two-digit hour AND a two-digit
    /// minute both fit under the "under a day" branch. Reachable for a 7d
    /// window too: the format is a function of MINUTES REMAINING, not which
    /// window sent them, so a 7d window with under a day left prints the same
    /// hour-tier shape a 5h window does.
    static let resetCaptionWidth: CGFloat = 66

    /// The width every quota row reserves for the account's cost figure, on
    /// a card that carries one.
    ///
    /// RESERVED on every row, drawn on the first. The bar is the row's `1fr`
    /// column, so a figure that took width from one row alone left that row's
    /// bar shorter than the two below it, and equal percentages then drew
    /// unequal ink inside ONE card — the exact confusion `QuotaRow`'s own
    /// doc-comment says the full-width bar exists to remove.
    /// Sized by MEASURING the strings it carries, at Comfortable's 12 pt:
    /// `$12,345 · 120M` is 87.2 pt and `Team Standard` is 85.1 pt. It was 68,
    /// which fits `$540 · 1.5M` (66.9) and nothing else — `$1,190 · 3.1M` is
    /// 73.5 and was drawing as `$1,1…3.1M`, eliding the middle of a figure
    /// whose middle is the number.
    ///
    /// It does not grow past that. The column is a FIXED width shared by every
    /// quota row, so every point given to it comes out of BOTH bars on EVERY
    /// card: at 112 pt, for a window caption, the bar went from a measured
    /// 154 pt to 48 pt. A string too long for this column belongs on a line of
    /// its own, not in it — `QuotaTailWidthTests` is the gate that catches one
    /// before it ships.
    static let usageTailWidth: CGFloat = 88
    static var quotaMarginTop: CGFloat { compact ? 2 : 3 }
    static var barHeight: CGFloat { compact ? 6 : 7 }
    static let barRadius: CGFloat = 4
    static let barTrackAlpha: Double = 0.08
    static let barMinWidth: CGFloat = 2
    /// `.bar.capped{max-width:110px}` — the BY TOOL bars only.
    static let barCappedWidth: CGFloat = 110

    // MARK: - Pill (`.pill`)

    static let pillFontSize: CGFloat = 10.5
    /// `letter-spacing:.06em` at 10.5 pt.
    static let pillTracking: CGFloat = 0.06 * 10.5
    static let pillPaddingV: CGFloat = 2
    static let pillPaddingH: CGFloat = 7
    static let pillRadius: CGFloat = 7
    static let pillBorderWidth: CGFloat = 1
    /// `.pill.ok{border-color:rgba(95,208,122,.35)}` and `.4` for the others.
    static let pillBorderAlpha: Double = 0.4
    static let pillGap: CGFloat = 6

    // MARK: - Status dot (`.dot`)

    static let dotSize: CGFloat = 8
    static let dotTrailingGap: CGFloat = 6
    /// `box-shadow:0 0 0 3px rgba(...,.18)` — a 3 pt ring, so a 14 pt halo.
    static let dotHaloWidth: CGFloat = 3
    static var dotHaloSize: CGFloat { dotSize + 2 * dotHaloWidth }

    // MARK: - Session block (`.sess`) and sparkline (`.spark`)

    static let sessMarginTop: CGFloat = 8
    static let sessMarginBottom: CGFloat = 2
    static let sessPaddingLeft: CGFloat = 10
    static let sessRuleWidth: CGFloat = 2
    /// `.spark{width:56px;height:14px}` — round 2 matched the mockup's
    /// current values; the earlier 64x18 was measured against a stale copy.
    static let sparklineWidth: CGFloat = 56
    static let sparklineHeight: CGFloat = 14
    static let sparklineStroke: CGFloat = 1.5

    // MARK: - Section head (`.sec`)

    static var sectionHeadMarginTop: CGFloat { compact ? 8 : 12 }
    static let sectionHeadMarginSide: CGFloat = 4
    static let sectionHeadMarginBottom: CGFloat = 4
    static let sectionHeadGlyph: CGFloat = 12
    static let sectionHeadGap: CGFloat = 6
    static let sectionHeadSize: CGFloat = 11
    static let sectionHeadTracking: CGFloat = 0.1 * 11

    // MARK: - Ring (`.ring`)

    /// 28 / 3.5, not the sheet's 34 / 4 (Gil, 2026-09-13, measuring his own
    /// Tools crop: the app's ring drew 39 pt against the mockup's 32, and three
    /// rows took 200 pt where the mockup takes 150). This supersedes the
    /// extracted CSS: the mockup's own `.ring` is 34 in a row whose two text
    /// lines are taller than ours, and a ring sized from the stylesheet rather
    /// than from the row it sits in is what made the rows grow around it.
    static var ringSize: CGFloat { compact ? 24 : 28 }
    /// The ring on a Tools-tab RUNNING NOW row, which is smaller than every
    /// other ring on the panel — 22, the mockup's own
    /// `.tool .ring{width:22px}` (`docs/design/panel-tabs-mockup.html:263`),
    /// which overrides its generic `.ring{width:34px}` for exactly this row.
    ///
    /// The app's 28 came from that generic rule, measured down from 34 (Gil,
    /// 2026-09-13); the per-row override was missed. It matters now because
    /// this row grew a cpu/memory clause and a ✕: at 28 the line could not
    /// hold the session name, the figure and the trailing group together, and
    /// the name or the memory figure clipped. Six points here and two from the
    /// gap below are what pay for the text (Gil, 2026-09-14).
    static var toolRowRingSize: CGFloat { compact ? 20 : 22 }
    static let ringStroke: CGFloat = 3.5
    static let ringTrackAlpha: Double = 0.08
    /// The Bash tool's own timeout — the denominator the ring fills toward, and
    /// the figure the RUNNING NOW section head states out loud.
    static let toolTimeoutSeconds: Double = 600
    /// Inside this many seconds of ``toolTimeoutSeconds`` the ring turns `bad`
    /// and the row says how long is left (Gil, 2026-09-13: "red plus
    /// `· 20s to timeout` within 60 s of the 600 s limit").
    static let toolTimeoutWarnSeconds: Double = 60

    // MARK: - Kill (`✕`)

    /// The ✕'s hit target. 24, not the 22 pt `docs/design/tools-tab.md` calls
    /// the minimum: that document's own rule takes the ✕ to 24 because macOS'
    /// pointer minimum is 24 and this is the one DESTRUCTIVE control on the
    /// tab. A hit target smaller than the glyph's own confidence is how a
    /// mis-click kills the wrong command.
    static let killHitTarget: CGFloat = 24
    /// The glyph inside that target. Smaller than the box on purpose: the box
    /// is what the pointer must hit, the glyph is what the eye must not be
    /// dominated by — a running row is about its command, not about the way
    /// to end it.
    static let killGlyph: CGFloat = 10

    /// The fixed column every row's trailing content occupies — the ring and
    /// its duration on Tools, the sparkline or the status on Sessions.
    ///
    /// A column sized per row from its own content is what made the durations
    /// zig-zag down the tab. Two widths, one per tab, because the widest
    /// trailing string differs: Tools prints "19,913 · median 2.1s" and Sessions
    /// prints "2 running · oldest 9m 40s". A single width wide enough for both
    /// would eat the session NAME, which is the one string on the row that has
    /// to stay readable.
    /// Tools: the ring, the gap, and the duration's own right-aligned column.
    /// Nothing wider — every point past those three is a point taken from the
    /// command the row is about, which is the string a reader is scanning.
    static var trailingColumnWidth: CGFloat { ringSize + rowGap + durationColumnWidth }
    /// The duration's own sub-column inside the Tools column, right-aligned so
    /// every duration ends at the card's content edge and the ring beside it
    /// starts at one x on every row.
    static let durationColumnWidth: CGFloat = 60
    /// A row carrying a ``ProgressRing`` is at least the ring plus the gap that
    /// keeps two rings from touching. `.row`'s own line box is 21 pt and the
    /// ring is 34, so without this the rings of consecutive rows overlap —
    /// measured at 74 px of ring inside a 68 px row.
    ///
    /// The row is its own two text lines, not the ring: a 15 pt mono line and a
    /// 12 pt mute line are 21 + 17 = 38 pt, and the 28 pt ring fits inside that
    /// with room to spare. Sizing the row from the ring instead is what put
    /// 67 pt between rows the mockup sets 56 pt apart.
    static var ringRowMinHeight: CGFloat { lineHeight(monoSize) + lineHeight(muteSize) }

    /// A command head gets TWO lines of its own, full width, before it
    /// ellipsises — `docs/design/tools-tab.md`'s own layout rule, from the
    /// "essential text truncation" guideline: the command is the
    /// distinguishing text, and cutting it at 36 characters to fit beside a
    /// ring hides the one thing that tells two calls apart.
    static let commandLineLimit = 2
    /// The count column on a TIMED OUT TODAY row. Wide enough for three
    /// digits: a day past 999 timeouts in one class is a different
    /// conversation.
    static let timeoutCountWidth: CGFloat = 26
    /// The BY TOOL footer line — 11 pt, the section-head size, because that
    /// is what it now is: a caption under the tab rather than a card in it.
    static let byToolLineSize: CGFloat = 11

    // MARK: - Group (`.grp`)

    static let groupPaddingTop: CGFloat = 12
    static let groupPaddingSide: CGFloat = 8
    static let groupPaddingBottom: CGFloat = 4
    /// `.grp.collapsed{padding-bottom:8px}`.
    static let groupPaddingBottomCollapsed: CGFloat = 8
    static let groupMarginTop: CGFloat = 20
    static let groupMarginBottom: CGFloat = 8
    static let groupRadius: CGFloat = 16
    static let groupStroke: CGFloat = 1.5
    /// `.grp .card{margin:6px 0}`.
    static let groupCardGap: CGFloat = 6
    /// The legend sits at `top:-7px; left:calc(var(--n0) + 4px)` with `--n0:10px`,
    /// and the stroke is masked out for a 9 px band behind it.
    ///
    /// The legend is NOT offset by a constant: `top:-7px` is the CSS's way of
    /// writing "centre an 11 pt line box on the 1.5 pt stroke" (a 15.4 pt line
    /// box lifted 7 pt sits 0.7 pt below the edge, i.e. centred), and a SwiftUI
    /// label whose height is its own text metrics is a different number.
    /// ``GroupBox`` lifts it by half its MEASURED height instead, which is the
    /// same intent and survives a font change; the constant put the legend
    /// 1.75 pt high on the first render.
    static let legendNotchStart: CGFloat = 10
    static let legendNotchPadding: CGFloat = 4
    static let legendMaskBand: CGFloat = 9
    /// How wide the stroke's notch is for a legend that measured `width`: the
    /// mask's `--n1`, clamped so a zero-width legend cannot ask for a negative
    /// band. Here rather than in the view because it is the sheet's geometry.
    static func legendNotchWidth(forLegendWidth width: CGFloat) -> CGFloat {
        max(0, legendNotchStart + legendNotchPadding * 2 + width - legendNotchStart)
    }

    /// How far a legend of height `height` is lifted so it sits centred on the
    /// stroke — the intent behind the CSS's `top:-7px` (see the note above).
    static func legendLift(forLegendHeight height: CGFloat) -> CGFloat {
        -height / 2
    }

    static let legendFontSize: CGFloat = 11
    static let legendTracking: CGFloat = 0.08 * 11
    /// 12, not the CSS's 11: the swatch reads as a rounded square beside an
    /// 11 pt uppercase legend and at 11 it sat visibly smaller than the cap
    /// height next to it (Gil, 2026-09-13).
    static let legendGlyph: CGFloat = 12
    static let legendGap: CGFloat = 6

    // MARK: - Button (`.btn` / `.more`)

    static let buttonRadius: CGFloat = 7
    static let buttonMinHeight: CGFloat = 28
    static let buttonPaddingV: CGFloat = 5
    static let buttonPaddingH: CGFloat = 11
    /// The border again (see ``cardInsetV``).
    static var buttonInsetV: CGFloat { buttonPaddingV + panelBorderWidth }
    static var buttonInsetH: CGFloat { buttonPaddingH + panelBorderWidth }
    static let buttonFillAlpha: Double = 0.10
    static let buttonFontSize: CGFloat = 13
    static let buttonGap: CGFloat = 8
    /// `.acts{margin-top:12px}` — the action row's own top margin, so it sits
    /// as far under the last card as the tabs sit above the list (the
    /// mockup's `.card{margin:14px 0}` already gives the card its half of
    /// that gap; this is the other half).
    static let actsMarginTop: CGFloat = 12
    /// `.btn.danger{border-color:rgba(239,107,107,.38)}`.
    static let dangerBorderAlpha: Double = 0.38
    /// `.more` — the disclosure control: full width, 12.5 pt/600, its own 6 pt
    /// top margin and a 12 pt chevron.
    static let discFontSize: CGFloat = 12.5
    static let discMarginTop: CGFloat = 6
    static let discGlyph: CGFloat = 12
    static let discRadius: CGFloat = 8
    static let discPaddingV: CGFloat = 5
    static let discPaddingH: CGFloat = 10
    static var discInsetV: CGFloat { discPaddingV + panelBorderWidth }
    static var discInsetH: CGFloat { discPaddingH + panelBorderWidth }
    /// `.aside` — "3 accounts have no sessions".
    static let asideFontSize: CGFloat = 12
    static let asidePaddingTop: CGFloat = 6

    // MARK: - Footer (`.foot`)

    static let footerMarginTop: CGFloat = 10
    static let footerPaddingTop: CGFloat = 8
    static let footerRuleWidth: CGFloat = 1
    static let footerGap: CGFloat = 10
    static let footerGlyph: CGFloat = 12

    // MARK: - Type

    static let titleSize: CGFloat = 17
    static let titleTracking: CGFloat = -0.01 * 17
    static let freshnessSize: CGFloat = 12.5
    static var summarySize: CGFloat { compact ? 13 : 15 }
    static var tabLabelSize: CGFloat { compact ? 12 : 12.5 }
    static let tabLabelTracking: CGFloat = 0.02 * 12.5
    static let badgeSize: CGFloat = 11
    static var nameSize: CGFloat { compact ? 13 : 15 }
    static let nameTracking: CGFloat = -0.005 * 15
    static var dimSize: CGFloat { compact ? 12 : 13 }
    static var muteSize: CGFloat { compact ? 11 : 12 }
    /// The cpu/memory figure on a Tools-tab running row — the mockup's own
    /// `.tool .stat{font-size:11px}`
    /// (`docs/design/panel-tabs-mockup.html:264`), a point under the `.who`
    /// text it sits beside.
    ///
    /// Not a taste choice and not a squeeze: it is the size that element is
    /// specified at, and it was missed when this row was first drawn. Measured
    /// need, with the name whole and the 22 pt ring: the line has 324.5 pt and
    /// the figure at `muteSize` wants 325.8, at this size 320.1.
    static var toolRowStatSize: CGFloat { compact ? 10 : 11 }
    static let monoSize: CGFloat = 12
    static let footerSize: CGFloat = 12.5

    // MARK: - Motion

    /// `scale .96 over 120 ms` on press; `spring, damping 1.0, response 0.3` for a
    /// tab switch or an expand.
    static let pressScale: CGFloat = 0.96
    /// `.more:hover{background:rgba(255,255,255,.07)}` — and the same +.07 the
    /// sheet's other hovers are: `.btn` goes `.10` to `.17`, `.more` `0` to
    /// `.07`. Added OVER whatever fill a control already has, so one rule
    /// reproduces both rather than a hover value per control.
    static let hoverFillAlpha: Double = 0.07
    /// `transition:background-color .15s cubic-bezier(.2,0,0,1)`. Inside the
    /// sheet's `@media (prefers-reduced-motion:no-preference)` block, which is
    /// why Reduce Motion drops the transition and keeps the hover.
    static let hoverDuration: Double = 0.15
    static let springResponse: Double = 0.3
    static let springDamping: Double = 1.0

    // MARK: - The two colours the gated palette has no name for

    /// `--trend`, the sparkline stroke. Decorative: it draws a 1.5 pt line, never
    /// text, so it carries no contrast obligation and is not a palette token.
    static let trend = Color(red: 0x7f / 255, green: 0xb2 / 255, blue: 0xff / 255)
    /// `--info`, the UNMEASURED pill and the Agent bar. `Tok.unmeasured` is this
    /// role in the gated palette and is what the pill and bar actually use; this
    /// value is kept beside `trend` only so the sheet's own two extra roles are
    /// both named in one place.
    static let info = Color(red: 0x6a / 255, green: 0xa9 / 255, blue: 0xff / 255)

    // MARK: - Line boxes

    /// `body{font:15px/1.4}` — the mockup's ONE line-height, inherited by every
    /// line on the panel.
    ///
    /// This is the number eleven adaptation rounds did not have. A browser gives
    /// a 15 pt line a 21 pt box; SwiftUI gives it ~18 pt, so a transcription that
    /// gets every padding right still draws every card 3 pt per row short — and
    /// the previous round compensated by padding `.row` by 4, which the sheet
    /// only does for `.sess .row` and which then overshot in the other
    /// direction. One factor, applied to each text role's own size.
    static let lineHeightFactor: CGFloat = 1.4

    /// The line box a run of `size` pt text occupies, rounded the way a layout
    /// engine rounds it.
    static func lineHeight(_ size: CGFloat) -> CGFloat {
        (size * lineHeightFactor).rounded()
    }

    /// What to add BETWEEN two wrapped lines of `size` pt text so the pair
    /// occupies the same box the browser gives it.
    ///
    /// ``lineHeight(_:)`` alone only fixes a single line (it is a minimum on the
    /// frame); a `Text` that wraps stacks its own natural line height twice and
    /// comes out short again. The natural height is asked of the font rather
    /// than guessed at a factor, so this survives a font change and a
    /// Larger-Text setting.
    static func lineSpacing(_ size: CGFloat) -> CGFloat {
        let font = NSFont.systemFont(ofSize: size)
        let natural = font.ascender - font.descender + font.leading
        return max(0, lineHeight(size) - natural)
    }

    /// A `.card`'s `.row`: its tallest child is the 15 pt name, so the row is
    /// that name's line box. Every line INSIDE that row gets the same box —
    /// a CSS line box is set by its block's strut, not by the smallest span on
    /// it, so a 12 pt plan wrapping under a 15 pt name still occupies 21 pt.
    static var rowLineHeight: CGFloat { lineHeight(nameSize) }

    /// What a block's top margin becomes when it follows the segmented strip.
    ///
    /// Adjacent CSS margins COLLAPSE to the larger of the two: the strip's
    /// `margin-bottom:12` and the first card's `margin-top:14` make one 14 pt
    /// gap, not 26. SwiftUI adds paddings, which is how the first v4 render put
    /// 29 pt of nothing under the tabs against the mockup's 15.
    static func marginAfterStrip(_ own: CGFloat) -> CGFloat {
        max(own, segMarginBottom) - segMarginBottom
    }

    // MARK: - Derived helpers

    /// `font-variant-numeric:tabular-nums` is set on `.panel`, so every number on
    /// this panel is tabular. Applied through one helper rather than remembered
    /// at forty call sites.
    static func font(_ size: CGFloat, _ weight: Font.Weight = .regular) -> Font {
        .system(size: size, weight: weight).monospacedDigit()
    }

    static func mono(_ size: CGFloat) -> Font {
        .system(size: size, design: .monospaced).monospacedDigit()
    }

    /// A group's identity colour, as SwiftUI. Never a status hue — the sheet is
    /// explicit that group identity is "outline + legend only".
    static func groupColor(_ rgb: GroupTagColor.RGB) -> Color {
        Color(red: rgb.red, green: rgb.green, blue: rgb.blue)
    }
}
