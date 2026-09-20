import AppKit

/// The whole menu-bar image: one coffee cup, this app draws itself.
///
/// ## Why the app composes this instead of handing SwiftUI a view
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
/// ## One glyph, not two
///
/// This used to be a capacity gauge with a second, separate cup mark beside it
/// for keep-awake. It is now the cup alone, carrying both facts on one glyph:
///
///  - **Fill level** — how much of the cup is drawn as liquid, clipped to the
///    bottom `fraction` of the canvas under the cup's own silhouette. This is
///    the fleet's capacity (``StatusPoller/PollState/capacityFraction``), and
///    it is present in every ``Tint`` — an empty cup still reads as a cup.
///  - **Tint** — ``Tint/template`` (plain, system-coloured, exactly like every
///    other stock menu-bar icon) when keep-awake is off and nothing is wrong;
///    ``Tint/awake(_:)`` while keep-awake holds the Mac up; ``Tint/near(_:)``
///    when no account is ready and at least one is close; ``Tint/failed(_:)``
///    when the last poll did not answer at all. Colour is deliberately a
///    single channel here — unlike the old gauge/cup pair, there is only one
///    glyph left to carry it, so the caller (``MenuBarShell/cupTint(for:awake:)``)
///    picks exactly one `Tint` per poll, never blends two.
///
/// ## The catch, and how a coloured tint dodges it
///
/// A non-template image is drawn exactly as authored, so it stops getting the
/// system's automatic menu-bar tinting and would freeze at whatever colour was
/// baked in — right in one appearance and wrong in the other, UNLESS the
/// dynamic colour resolves inside the drawing handler, which runs at draw
/// time rather than at construction. Every coloured branch below rebuilds its
/// tinted symbol inside the closure for exactly that reason — the identical
/// rule ``KeepAwakeGlyph`` documented for the mark this type replaces.
///
/// `Tint.template` needs none of that: it stays `isTemplate = true` end to
/// end, so the menu bar itself supplies the colour, in both appearances and
/// over a light wallpaper, for free. Do not regress it to a hand-tinted image
/// to make the branches look alike.
public enum MenuBarMark {

    /// The cup's outline — SF Symbol, so a `.template` mark renders exactly
    /// like every other stock menu-bar glyph and follows the menu bar's own
    /// tint automatically.
    public static let symbolName = "cup.and.saucer"
    /// The filled variant, used only as the liquid's silhouette: drawn inside
    /// a clip rect covering the bottom `fraction` of the canvas, so only the
    /// part of it that falls both inside the cup AND below the fill line ever
    /// reaches the canvas. Same symbol ``KeepAwakeGlyph`` names, so the two
    /// files cannot silently pick different cups.
    static let filledSymbolName = KeepAwakeGlyph.symbolName

    /// What VoiceOver says about the menu-bar item. It is the one surface with
    /// no room for a label, so this is the only place the state is spoken.
    ///
    /// A Mac waiting on an answer is spoken FIRST, in
    /// ``PeerAdmission/knockBarSentence(count:)``'s own words, because it is
    /// the one thing here that is waiting on a person rather than reporting on
    /// a fleet. Capacity follows it, unchanged, and at zero knocks the
    /// sentence is byte-identical to what it has always been.
    public static func accessibilityDescription(awake: Bool, knocks: Int = 0) -> String {
        let capacity =
            awake
            ? "tcr fleet capacity. \(KeepAwakeGlyph.accessibilityDescription)."
            : "tcr fleet capacity"
        guard let asking = PeerAdmission.knockBarSentence(count: knocks) else { return capacity }
        return "\(asking) \(capacity)"
    }

    /// Which colour the cup wears. Kept as data here, decided by the caller
    /// from `PollState` and the keep-awake toggle, so this type stays
    /// decoupled from `PollState`'s cases — the same reason `KeepAwakeGlyph`
    /// took its tint as a parameter rather than reaching for `Tok` itself.
    public enum Tint: Equatable {
        /// Plain: `isTemplate = true`, no colour of its own. Do not hand-tint
        /// this to match the other branches — the whole point of staying
        /// template is the free system tinting it buys.
        case template
        /// Keep-awake is on: the whole cup, outline and liquid alike, in this
        /// colour (`Tok.awakeNSColor` at the call site).
        case awake(NSColor)
        /// No account ready, at least one near its limit
        /// (`Fleet.capacityGlyphState == .near`).
        case near(NSColor)
        /// The last poll did not answer at all. Callers pass `fraction: nil`
        /// alongside this case — there is no reading to draw liquid for, only
        /// an outline in this colour.
        case failed(NSColor)

        /// `nil` for `.template`, the dynamic colour otherwise.
        var color: NSColor? {
            switch self {
            case .template: return nil
            case .awake(let c), .near(let c), .failed(let c): return c
            }
        }
    }

    /// The image for `NSStatusBarButton.image`.
    ///
    /// - Parameter fraction: the cup's fill level, `0...1`. `nil` (an all-
    ///   disabled fleet, or no fleet read yet) draws exactly like `0` — an
    ///   empty cup, never a fault-coloured one; the fault, if there is one,
    ///   is `tint`'s job. Clamped either way, so a caller passing a stale
    ///   `1.4` from a fleet that shrank mid-poll cannot draw liquid above the
    ///   rim.
    /// - Parameter tint: see ``Tint``.
    ///
    /// `nil` only when the cup symbol itself cannot be created — that is a
    /// missing SF Symbol, which the caller has to notice rather than paper
    /// over with an empty status item.
    /// - Parameter knocks: how many Macs are waiting on an answer, for the
    ///   spoken description alone. The knock MARK itself is a separate glyph
    ///   in the status item's title (`MenuBarShell.updateMark`), not part of
    ///   this cup: it has to be drawn whether or not the counts label is on,
    ///   and the cup is a reading of the fleet and nothing else.
    public static func image(fraction: Double?, tint: Tint, knocks: Int = 0) -> NSImage? {
        guard
            let outline = NSImage(
                systemSymbolName: symbolName,
                accessibilityDescription: accessibilityDescription(awake: false))
        else { return nil }
        outline.isTemplate = true

        guard
            let filled = NSImage(
                systemSymbolName: filledSymbolName,
                accessibilityDescription: accessibilityDescription(awake: false))
        else { return nil }
        filled.isTemplate = true

        let size = outline.size
        let rect = NSRect(origin: .zero, size: size)
        let clampedFraction = CGFloat(min(1, max(0, fraction ?? 0)))

        let composed = NSImage(size: size, flipped: false) { _ in
            Self.draw(
                rect: rect, outline: outline, filled: filled, fraction: clampedFraction,
                color: tint.color)
            return true
        }
        // Only `.template` stays a template end to end — every coloured
        // branch draws an arbitrary hue the menu bar must not re-tint.
        composed.isTemplate = tint == .template
        let awake: Bool
        if case .awake = tint { awake = true } else { awake = false }
        composed.accessibilityDescription = accessibilityDescription(awake: awake, knocks: knocks)
        return composed
    }

    /// Liquid first, rim on top — so the rim always reads as a clean outline
    /// rather than being partly overpainted by the fill.
    ///
    /// `color == nil` (the `.template` branch) draws both symbols exactly as
    /// fetched, still `isTemplate = true`; a non-`nil` color rebuilds a tinted,
    /// non-template copy of each symbol INSIDE this call — which itself runs
    /// inside the image's drawing handler — so a dynamic `NSColor` resolves
    /// against the appearance current at draw time, not whichever appearance
    /// happened to be current when the mark was composed.
    private static func draw(
        rect: NSRect, outline: NSImage, filled: NSImage, fraction: CGFloat, color: NSColor?
    ) {
        if fraction > 0 {
            NSGraphicsContext.saveGraphicsState()
            NSRect(x: 0, y: 0, width: rect.width, height: rect.height * fraction).clip()
            let body: NSImage
            if let color {
                body =
                    filled.withSymbolConfiguration(NSImage.SymbolConfiguration(paletteColors: [color]))
                    ?? filled
                body.isTemplate = false
            } else {
                body = filled
            }
            body.draw(in: rect, from: .zero, operation: .sourceOver, fraction: 1)
            NSGraphicsContext.restoreGraphicsState()
        }

        let rim: NSImage
        if let color {
            rim =
                outline.withSymbolConfiguration(NSImage.SymbolConfiguration(paletteColors: [color]))
                ?? outline
            rim.isTemplate = false
        } else {
            rim = outline
        }
        rim.draw(in: rect, from: .zero, operation: .sourceOver, fraction: 1)
    }
}
