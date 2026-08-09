import AppKit

/// The tinted mark that sits beside the capacity gauge while keep-awake is on.
///
/// ## Why an `NSImage` and not `Image(systemName:).foregroundStyle(…)`
///
/// A status item draws its image as a **template**: macOS re-renders it in the
/// menu bar's own colour so that it reads on any wallpaper and in either
/// appearance, which strips whatever tint the view asked for. `isTemplate =
/// false` is the documented opt-out, and it is a property of `NSImage` — there
/// is nowhere to set it on a SwiftUI `Image(systemName:)`. So the mark is built
/// here and handed over as `Image(nsImage:)`.
///
/// ## What is measured, and what is not
///
/// `AwakeControllerTests` rasterises the returned image and asserts two facts
/// about it: `isTemplate == false`, and that a pixel inside the glyph actually
/// carries the tint that was asked for. That proves the image **this app hands
/// to the status item** carries colour, which is the half that is checkable
/// without a screen.
///
/// It does not prove the status item honours it, and nothing available here
/// can: reading back the real menu bar needs `screencapture`, which needs
/// Screen Recording, which is not granted on the machine this was written on.
/// That last step is a human looking at their own menu bar.
///
/// Which is the reason colour is the *second* channel and not the only one —
/// see `MenuBarLabel`. If the tint is lost, a glyph that is present or absent
/// still says whether the mode is on.
///
/// ## Why it lives in `TcrBarCore`
///
/// The test target links `TcrBarCore` only (`Package.swift`), so an image
/// builder sitting in `TcrBar` next to the tokens could not be tested at all.
/// The tint is a parameter for the matching reason: `Tok` stays the one place a
/// colour is written down, and this file stays out of the view layer.
public enum KeepAwakeGlyph {
    /// A cup. The one mark for "caffeinated" that needs no legend, and it is
    /// nothing like a gauge — the two are told apart by silhouette before
    /// colour is involved at all.
    public static let symbolName = "cup.and.saucer.fill"

    /// What VoiceOver says. The menu bar is the one surface with no room for a
    /// label, so this is the only place the state is spoken.
    public static let accessibilityDescription = "Keeping this Mac awake"

    /// `nil` when the symbol cannot be created. Callers fall back to the plain
    /// template glyph rather than drawing nothing: losing the tint costs one of
    /// two channels, losing the glyph costs the signal.
    public static func image(tint: NSColor) -> NSImage? {
        guard
            let symbol = NSImage(
                systemSymbolName: symbolName,
                accessibilityDescription: accessibilityDescription)
        else { return nil }

        let tinted = symbol.withSymbolConfiguration(
            NSImage.SymbolConfiguration(paletteColors: [tint])) ?? symbol
        tinted.isTemplate = false
        // `withSymbolConfiguration` returns a new image; carry the description
        // across rather than assuming it was copied.
        tinted.accessibilityDescription = accessibilityDescription
        return tinted
    }
}
