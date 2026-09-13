import AppKit
import XCTest

@testable import TcrBarCore

/// The menu-bar image, checked at the level a unit test can reach: the `NSImage`
/// this app hands to the status button.
///
/// What it cannot reach is whether the *button* honours what the image says —
/// that needs a real `NSStatusItem` and a rasterisation off it, which is what
/// `TcrBar --shell-probe` is for. These tests are the cheap half that runs in
/// `swift test`; the probe is the gate.
@MainActor
final class MenuBarMarkTests: XCTestCase {

    /// Pure saturated red rather than `Tok.awakeNSColor`: the assertions below
    /// are about colour surviving at all, so they must not be able to pass on a
    /// near-grey, and a palette token is free to change.
    private let loudTint = NSColor(srgbRed: 1, green: 0, blue: 0, alpha: 1)

    /// `.template` stays a template, which is what buys the menu bar's automatic
    /// tinting — correct in both appearances and over a light wallpaper, for
    /// free. Hand-tinting it to match the coloured branches would throw that away.
    func testTemplateTintIsATemplate() throws {
        let image = try XCTUnwrap(MenuBarMark.image(fraction: 0.5, tint: .template))
        XCTAssertTrue(image.isTemplate)
    }

    /// A template image is re-rendered in the menu bar's own colour, which strips
    /// the tint. `isTemplate = false` is the documented opt-out and the single
    /// property that makes the colour channel possible.
    func testEveryColouredTintIsNotATemplate() throws {
        for tint: MenuBarMark.Tint in [.awake(loudTint), .near(loudTint), .failed(loudTint)] {
            let image = try XCTUnwrap(MenuBarMark.image(fraction: 0.5, tint: tint))
            XCTAssertFalse(image.isTemplate, "\(tint) must not stay a template")
        }
    }

    /// The point of the whole rebuild: a coloured mark really carries colour.
    func testAPixelInAColouredMarkCarriesTheTint() throws {
        let image = try XCTUnwrap(MenuBarMark.image(fraction: 1, tint: .awake(loudTint)))
        let scan = try XCTUnwrap(rasterise(image, in: XCTUnwrap(NSAppearance(named: .darkAqua))))

        // A positive control on the probe itself: with nothing rasterised the
        // colour assertion below would be vacuous rather than failing.
        XCTAssertGreaterThan(scan.opaque, 0, "nothing was rasterised — the scan proves nothing")
        XCTAssertGreaterThan(scan.tinted, 0, "no red in the mark — the tint was dropped")
    }

    /// And `.template` does not, which is the negative control. Without it the
    /// test above passes just as happily on a mark that is red in every tint —
    /// i.e. on a menu bar that cannot tell keep-awake-on from off.
    func testTheTemplateTintCarriesNoTint() throws {
        let image = try XCTUnwrap(MenuBarMark.image(fraction: 1, tint: .template))
        let scan = try XCTUnwrap(rasterise(image, in: XCTUnwrap(NSAppearance(named: .darkAqua))))

        XCTAssertGreaterThan(scan.opaque, 0, "nothing was rasterised — the scan proves nothing")
        XCTAssertEqual(scan.tinted, 0, "the template tint is coloured, so it cannot follow the menu bar")
    }

    /// The fill level: an empty cup draws no liquid at all — every opaque pixel
    /// is the outline's rim — while a full cup draws substantially more opaque
    /// pixels, the liquid filling the body the rim alone leaves hollow.
    func testAHigherFractionOpaquesMorePixelsThanAnEmptyCup() throws {
        let empty = try XCTUnwrap(MenuBarMark.image(fraction: 0, tint: .awake(loudTint)))
        let full = try XCTUnwrap(MenuBarMark.image(fraction: 1, tint: .awake(loudTint)))
        let emptyScan = try XCTUnwrap(rasterise(empty, in: XCTUnwrap(NSAppearance(named: .darkAqua))))
        let fullScan = try XCTUnwrap(rasterise(full, in: XCTUnwrap(NSAppearance(named: .darkAqua))))

        XCTAssertGreaterThan(
            fullScan.opaque, emptyScan.opaque,
            "a full cup must draw more ink than an empty one — the fill level is not being drawn")
    }

    /// `nil` fraction is documented to draw exactly like `0` — an all-disabled
    /// fleet or a poll with nothing loaded yet must never draw a fault-coloured
    /// cup that also happens to be full.
    func testANilFractionDrawsLikeZero() throws {
        let nilFraction = try XCTUnwrap(MenuBarMark.image(fraction: nil, tint: .awake(loudTint)))
        let zeroFraction = try XCTUnwrap(MenuBarMark.image(fraction: 0, tint: .awake(loudTint)))
        let a = try XCTUnwrap(rasterise(nilFraction, in: XCTUnwrap(NSAppearance(named: .darkAqua))))
        let b = try XCTUnwrap(rasterise(zeroFraction, in: XCTUnwrap(NSAppearance(named: .darkAqua))))
        XCTAssertEqual(a.opaque, b.opaque)
    }

    /// A fraction outside `0...1` (a stale reading from a fleet that shrank
    /// mid-poll, or a negative value `Double` does not itself forbid) must
    /// never draw more ink than a full cup or less than an empty one — the
    /// caller's contract is a sane image for any `Double`, not a crash or a
    /// cup that overflows its own rim. Measured, not merely asserted from the
    /// clamp's presence: with the `min`/`max` deliberately removed in a scratch
    /// build, both assertions here still held, because `NSRect.clip()` already
    /// normalises an over-tall or negative-height rect on this OS — so this
    /// test locks in the OUTPUT contract the function actually promises,
    /// leaving `min`/`max` as defensive belt-and-braces the platform's own
    /// clipping already backs up, not as the one thing standing between this
    /// function and a torn image.
    func testAnOutOfRangeFractionStaysBoundedByEmptyAndFull() throws {
        let empty = try XCTUnwrap(MenuBarMark.image(fraction: 0, tint: .awake(loudTint)))
        let full = try XCTUnwrap(MenuBarMark.image(fraction: 1, tint: .awake(loudTint)))
        let emptyScan = try XCTUnwrap(rasterise(empty, in: XCTUnwrap(NSAppearance(named: .darkAqua))))
        let fullScan = try XCTUnwrap(rasterise(full, in: XCTUnwrap(NSAppearance(named: .darkAqua))))

        for outOfRange: Double in [-0.5, 1.4] {
            let image = try XCTUnwrap(MenuBarMark.image(fraction: outOfRange, tint: .awake(loudTint)))
            let scan = try XCTUnwrap(rasterise(image, in: XCTUnwrap(NSAppearance(named: .darkAqua))))
            XCTAssertGreaterThanOrEqual(scan.opaque, emptyScan.opaque, "\(outOfRange) drew less than empty")
            XCTAssertLessThanOrEqual(scan.opaque, fullScan.opaque, "\(outOfRange) drew more than full")
        }
    }

    /// The claim a coloured branch rests on: a non-template image is drawn
    /// exactly as authored, so a colour baked in at composition time would be
    /// wrong in the other appearance — unless the dynamic colour is resolved
    /// inside the drawing handler, which runs at draw time.
    ///
    /// Asserted on one image drawn twice, not on two images: that is the
    /// stronger statement, and it is the one that fails if `NSImage` ever
    /// caches the first raster.
    func testADynamicTintReResolvesPerAppearance() throws {
        // `Tok.awakeNSColor`'s own shape, without depending on `TcrBar`: two
        // different fixed hues, chosen dynamically. `NSColor(name:dynamicProvider:)`
        // is the same primitive the real token is built on.
        let dynamic = NSColor(name: nil) { appearance in
            appearance.bestMatch(from: [.aqua, .darkAqua]) == .darkAqua
                ? NSColor(srgbRed: 0, green: 1, blue: 0, alpha: 1)
                : NSColor(srgbRed: 1, green: 0, blue: 1, alpha: 1)
        }
        let image = try XCTUnwrap(MenuBarMark.image(fraction: 1, tint: .awake(dynamic)))
        let light = try XCTUnwrap(rasterise(image, in: XCTUnwrap(NSAppearance(named: .aqua))))
        let dark = try XCTUnwrap(rasterise(image, in: XCTUnwrap(NSAppearance(named: .darkAqua))))

        XCTAssertGreaterThan(light.opaque, 0)
        XCTAssertGreaterThan(dark.opaque, 0)
        let distance =
            abs(light.meanColour.r - dark.meanColour.r) + abs(light.meanColour.g - dark.meanColour.g)
            + abs(light.meanColour.b - dark.meanColour.b)
        XCTAssertGreaterThan(
            distance, 0.3,
            "the same pixels came back the same colour in both appearances — the dynamic tint was baked in")
    }

    /// `nil` only when the cup symbol itself cannot be created — that is a
    /// missing SF Symbol, which the caller has to notice rather than paper over
    /// with an empty status item. Exercised through `KeepAwakeGlyph`'s own
    /// symbol name is not possible without a broken build, so this only pins
    /// the documented contract: a real symbol name always succeeds.
    func testARealFractionAndTintAlwaysProducesAnImage() {
        XCTAssertNotNil(MenuBarMark.image(fraction: 0, tint: .template))
        XCTAssertNotNil(MenuBarMark.image(fraction: 1, tint: .failed(loudTint)))
    }

    /// The menu bar has no room for a label, so this string is the only place the
    /// state is spoken. Both states have to say something, and they have to say
    /// different things.
    func testBothStatesAreSpokenAndTheyDiffer() throws {
        let off = MenuBarMark.accessibilityDescription(awake: false)
        let on = MenuBarMark.accessibilityDescription(awake: true)
        XCTAssertFalse(off.isEmpty)
        XCTAssertNotEqual(off, on)
        XCTAssertTrue(
            on.contains(KeepAwakeGlyph.accessibilityDescription),
            "the ON description must name the mode: \(on)")

        let image = try XCTUnwrap(MenuBarMark.image(fraction: 1, tint: .awake(loudTint)))
        XCTAssertEqual(image.accessibilityDescription, on)
    }

    // MARK: - Rasterising

    private struct Scan {
        var opaque = 0
        /// Pixels that carry the tint — i.e. any part of the cup drawn with a
        /// non-template colour.
        var tinted = 0
        /// Mean (r, g, b) of the opaque pixels, for telling one solid colour
        /// from another rather than merely "some colour, some other colour".
        var meanColour: (r: Double, g: Double, b: Double) = (0, 0, 0)
    }

    /// Draw at 2x under a chosen appearance and count. `performAsCurrentDrawingAppearance`
    /// is what makes the dynamic colours inside the drawing handler resolve
    /// against that appearance rather than the process's default.
    private func rasterise(_ image: NSImage, in appearance: NSAppearance) -> Scan? {
        let scale = 2
        let width = Int(image.size.width.rounded()) * scale
        let height = Int(image.size.height.rounded()) * scale
        guard width > 0, height > 0,
            let rep = NSBitmapImageRep(
                bitmapDataPlanes: nil, pixelsWide: width, pixelsHigh: height,
                bitsPerSample: 8, samplesPerPixel: 4, hasAlpha: true, isPlanar: false,
                colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0)
        else { return nil }

        appearance.performAsCurrentDrawingAppearance {
            NSGraphicsContext.saveGraphicsState()
            let context = NSGraphicsContext(bitmapImageRep: rep)
            NSGraphicsContext.current = context
            context?.cgContext.scaleBy(x: CGFloat(scale), y: CGFloat(scale))
            image.draw(
                in: NSRect(origin: .zero, size: image.size), from: .zero,
                operation: .sourceOver, fraction: 1)
            NSGraphicsContext.restoreGraphicsState()
        }

        var scan = Scan()
        var rSum = 0.0
        var gSum = 0.0
        var bSum = 0.0
        for x in 0..<rep.pixelsWide {
            for y in 0..<rep.pixelsHigh {
                guard let colour = rep.colorAt(x: x, y: y)?.usingColorSpace(.sRGB),
                    colour.alphaComponent > 0.35
                else { continue }
                scan.opaque += 1
                let r = colour.redComponent
                let g = colour.greenComponent
                let b = colour.blueComponent
                rSum += r
                gSum += g
                bSum += b
                // Not grey and not white/black — a template's own rendering —
                // counts as "carries a tint".
                let spread = max(r, g, b) - min(r, g, b)
                if spread > 0.15 {
                    scan.tinted += 1
                }
            }
        }
        if scan.opaque > 0 {
            scan.meanColour = (
                rSum / Double(scan.opaque), gSum / Double(scan.opaque), bSum / Double(scan.opaque)
            )
        }
        return scan
    }
}
