import AppKit

/// The whole menu-bar image: the capacity gauge, and — while keep-awake is on —
/// a tinted cup beside it, composed into ONE `NSImage` that this app draws
/// itself.
///
/// ## Why the app composes this instead of handing SwiftUI two views
///
/// It was a `MenuBarExtra` label, and a `MenuBarExtra` renders its label
/// **monochrome no matter what the image says**. Six label constructions were
/// each hosted in a real `MenuBarExtra` and rasterised off the real
/// `NSStatusBarButton` with `cacheDisplay(in:to:)`:
///
/// | label | opaque px | coloured px |
/// |---|---|---|
/// | `Image(nsImage:)`, `isTemplate = false` | 68 | 0 |
/// | `Image(nsImage:).renderingMode(.original)` | 68 | 0 |
/// | `Text("●").foregroundStyle(cyan)` | 158 | 0 |
/// | symbol pre-rasterised to a plain bitmap, `isTemplate = false` | 68 | 0 |
/// | `Text("☕")` (an emoji, i.e. a colour font) | 198 | 14 |
/// | `button.image = <tinted NSImage>` set on the button | 533 | 533 |
///
/// Only the last one carries an arbitrary colour, and it is not reachable from a
/// SwiftUI scene. So the app owns the `NSStatusItem`, and this type is what it
/// puts in `button.image`.
///
/// ## Two channels, deliberately, and the shape one comes first
///
/// Keep-awake is a SECOND mark beside the gauge, and the two never merge. The
/// gauge answers "can I work right now"; keep-awake answers "will this Mac stay
/// up while I do", and recolouring the gauge to say the second thing would
/// destroy the first.
///
///  - **Shape** — a glyph that is either there or not. The primary channel,
///    because it is the only one that cannot be taken away: it survives
///    greyscale, a red-green colour vision deficiency, and any future rendering
///    decision that templates the image after all. `Tokens.swift` records this
///    project already rejecting a palette where two states differed in hue alone.
///  - **Colour** — a genuine improvement when it works, which is the point of
///    owning the button.
///
/// ## The catch, and how the ON image dodges it
///
/// A non-template image is drawn exactly as authored, so the *gauge* half stops
/// getting the system's automatic menu-bar tinting and would freeze at whatever
/// colour was baked in — right in one appearance and wrong in the other. The
/// gauge is therefore drawn with `NSColor.labelColor` **inside**
/// `NSImage(size:flipped:drawingHandler:)`, whose handler runs at draw time, so
/// the dynamic colour resolves in the appearance that is actually current.
///
/// Measured rather than assumed, on this machine, with one `NSImage` drawn twice
/// under `performAsCurrentDrawingAppearance`: the handler ran both times (no
/// cached raster), the gauge's mean luma came out **0.000 under `.aqua` and
/// 0.847 under `.darkAqua`**, and the cup stayed **516/516 cyan** in both.
/// `--shell-probe` assertion 7 is that measurement, kept as a gate.
///
/// What this still does not buy: a non-template image does not invert while the
/// status button is highlighted (the popover open) the way a template one does,
/// so the ON mark keeps its own colours against the selection fill. That is a
/// cost of colour, it applies only while the panel is open, and it is why the
/// OFF image below stays a template.
public enum MenuBarMark {

    /// Gap between the gauge and the cup, in points.
    ///
    /// Matches `Tok.space1`, which cannot be referenced here: `Tok` lives in the
    /// app target and this type lives in the library so the tests can link it —
    /// the same reason `KeepAwakeGlyph` takes its tint as a parameter.
    public static let glyphSpacing: CGFloat = 2

    /// What VoiceOver says about the menu-bar item. It is the one surface with no
    /// room for a label, so this is the only place the state is spoken.
    public static func accessibilityDescription(awake: Bool) -> String {
        awake
            ? "tcr fleet capacity. \(KeepAwakeGlyph.accessibilityDescription)."
            : "tcr fleet capacity"
    }

    /// The image for `NSStatusBarButton.image`.
    ///
    /// - OFF: the gauge alone, `isTemplate = true`. Byte-for-byte what the app
    ///   drew before it owned the button — the menu bar tints it, and it adapts
    ///   to the appearance and to a light wallpaper for free. Do not regress this
    ///   to a hand-tinted image to make the two branches look alike.
    /// - ON: gauge and cup side by side in one non-template image.
    ///
    /// Composed at the symbols' natural height (the gauge is 15×15 and the cup
    /// 20×15 on this machine, against a `NSFont.menuBarFont(ofSize: 0).pointSize`
    /// of 13) rather than at an invented size.
    ///
    /// `nil` only when the gauge symbol itself cannot be created — that is a
    /// missing SF Symbol, which the caller has to notice rather than paper over.
    /// A missing *cup* degrades to the plain template gauge instead: losing the
    /// second mark costs one signal, drawing nothing at all costs the item.
    public static func image(
        gaugeSymbol: String,
        awake: Bool,
        awakeTint: NSColor
    ) -> NSImage? {
        guard
            let gauge = NSImage(
                systemSymbolName: gaugeSymbol,
                accessibilityDescription: accessibilityDescription(awake: false))
        else { return nil }
        // Set explicitly rather than relied on: it is what makes the OFF image
        // follow the menu bar, and it is what makes `draw` lay the glyph down as
        // a mask the tint below can fill.
        gauge.isTemplate = true

        guard awake, let cup = KeepAwakeGlyph.image(tint: awakeTint) else {
            gauge.accessibilityDescription = accessibilityDescription(awake: false)
            return gauge
        }

        let gaugeSize = gauge.size
        let cupSize = cup.size
        let size = NSSize(
            width: gaugeSize.width + glyphSpacing + cupSize.width,
            height: max(gaugeSize.height, cupSize.height))
        let gaugeRect = NSRect(
            x: 0, y: (size.height - gaugeSize.height) / 2,
            width: gaugeSize.width, height: gaugeSize.height)
        let cupRect = NSRect(
            x: gaugeSize.width + glyphSpacing, y: (size.height - cupSize.height) / 2,
            width: cupSize.width, height: cupSize.height)

        let composed = NSImage(size: size, flipped: false) { _ in
            gauge.draw(in: gaugeRect, from: .zero, operation: .sourceOver, fraction: 1)
            // Resolved HERE, not above: this closure runs on every draw, so a
            // dynamic colour picks up the appearance that is current at that
            // moment instead of the one that happened to be current when the
            // image was built.
            NSColor.labelColor.set()
            gaugeRect.fill(using: .sourceAtop)
            // Rebuilt inside the handler for the same reason — `awakeTint` is a
            // dynamic `NSColor` too, and a symbol configuration built once
            // outside would bake whichever appearance composed the image.
            KeepAwakeGlyph.image(tint: awakeTint)?
                .draw(in: cupRect, from: .zero, operation: .sourceOver, fraction: 1)
            return true
        }
        composed.isTemplate = false
        composed.accessibilityDescription = accessibilityDescription(awake: true)
        return composed
    }
}
