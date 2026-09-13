import AppKit

/// The cup symbol name and its VoiceOver string — the two facts about
/// keep-awake's mark that outlive the mark itself.
///
/// ## History: this used to compose the whole glyph
///
/// Before the coffee-mark rework there were two glyphs side by side — a
/// capacity gauge, and this file's tinted cup beside it while keep-awake was
/// on — and `image(tint:)` below built the second one. ``MenuBarMark`` now
/// draws ONE cup, whose fill level carries capacity and whose colour carries
/// keep-awake/near/failed, composing `symbolName` and its filled variant
/// itself rather than calling through to this type. `image(tint:)` stays,
/// still covered by its own tests, as the standalone builder for any caller
/// that wants a plain tinted cup without a fill level or a menu-bar canvas —
/// it is no longer on the path that draws the status item.
///
/// ## Why an `NSImage` and not `Image(systemName:).foregroundStyle(…)`
///
/// A status item draws its image as a **template**: macOS re-renders it in the
/// menu bar's own colour so that it reads on any wallpaper and in either
/// appearance, which strips whatever tint the view asked for. `isTemplate =
/// false` is the documented opt-out, and it is a property of `NSImage` — there
/// is nowhere to set it on a SwiftUI `Image(systemName:)`. So the mark is built
/// here.
///
/// Handing the result to SwiftUI was not enough, and that is measured: a
/// `MenuBarExtra` flattens its label to monochrome whatever the image says, for
/// every construction that was tried — the six-row table now lives in
/// ``MenuBarMark``, the type that composes the mark this app sets on the
/// status button itself.
///
/// ## Why it lives in `TcrBarCore`
///
/// The test target links `TcrBarCore` only (`Package.swift`), so an image
/// builder sitting in `TcrBar` next to the tokens could not be tested at all.
/// The tint is a parameter for the matching reason: `Tok` stays the one place a
/// colour is written down, and this file stays out of the view layer.
public enum KeepAwakeGlyph {
    /// A cup. The one mark for "caffeinated" that needs no legend, and now
    /// the whole menu-bar mark, not a second glyph beside a gauge.
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

        let tinted =
            symbol.withSymbolConfiguration(
                NSImage.SymbolConfiguration(paletteColors: [tint])) ?? symbol
        tinted.isTemplate = false
        // `withSymbolConfiguration` returns a new image; carry the description
        // across rather than assuming it was copied.
        tinted.accessibilityDescription = accessibilityDescription
        return tinted
    }
}
