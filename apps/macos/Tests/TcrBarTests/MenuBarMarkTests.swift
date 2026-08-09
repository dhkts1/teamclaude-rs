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

    private let gauge = "gauge.with.dots.needle.33percent"
    /// Pure saturated red rather than `Tok.awake`: the assertions below are about
    /// colour surviving at all, so they must not be able to pass on a near-grey,
    /// and a palette token is free to change.
    private let loudTint = NSColor(srgbRed: 1, green: 0, blue: 0, alpha: 1)

    /// The OFF mark stays a template, which is what buys the menu bar's automatic
    /// tinting — correct in both appearances and over a light wallpaper, for
    /// free. Hand-tinting it to match the ON branch would throw that away.
    func testOffMarkIsATemplate() throws {
        let image = try XCTUnwrap(
            MenuBarMark.image(gaugeSymbol: gauge, awake: false, awakeTint: loudTint))
        XCTAssertTrue(image.isTemplate)
    }

    /// A template image is re-rendered in the menu bar's own colour, which strips
    /// the tint. `isTemplate = false` is the documented opt-out and the single
    /// property that makes the colour channel possible.
    func testOnMarkIsNotATemplate() throws {
        let image = try XCTUnwrap(
            MenuBarMark.image(gaugeSymbol: gauge, awake: true, awakeTint: loudTint))
        XCTAssertFalse(image.isTemplate)
    }

    /// Two glyphs, not one recoloured glyph. The shape channel is the one that
    /// survives greyscale and a red-green colour vision deficiency, so the ON
    /// mark has to be *wider*, not merely different in hue.
    func testOnMarkIsWiderThanOffBecauseItCarriesASecondGlyph() throws {
        let off = try XCTUnwrap(
            MenuBarMark.image(gaugeSymbol: gauge, awake: false, awakeTint: loudTint))
        let on = try XCTUnwrap(
            MenuBarMark.image(gaugeSymbol: gauge, awake: true, awakeTint: loudTint))
        XCTAssertGreaterThan(on.size.width, off.size.width)
        XCTAssertEqual(on.size.height, off.size.height, accuracy: 0.5)
    }

    /// The point of the whole rebuild: the composed image really carries colour.
    func testAPixelInTheOnMarkCarriesTheTint() throws {
        let image = try XCTUnwrap(
            MenuBarMark.image(gaugeSymbol: gauge, awake: true, awakeTint: loudTint))
        let scan = try XCTUnwrap(rasterise(image, in: XCTUnwrap(NSAppearance(named: .darkAqua))))

        // A positive control on the probe itself: with nothing rasterised the
        // colour assertion below would be vacuous rather than failing.
        XCTAssertGreaterThan(scan.opaque, 0, "nothing was rasterised — the scan proves nothing")
        XCTAssertGreaterThan(scan.tinted, 0, "no red in the mark — the tint was dropped")
    }

    /// And the OFF mark does not, which is the negative control. Without it the
    /// test above passes just as happily on a mark that is red in both states —
    /// i.e. on a menu bar that cannot tell the two modes apart.
    func testTheOffMarkCarriesNoTint() throws {
        let image = try XCTUnwrap(
            MenuBarMark.image(gaugeSymbol: gauge, awake: false, awakeTint: loudTint))
        let scan = try XCTUnwrap(rasterise(image, in: XCTUnwrap(NSAppearance(named: .darkAqua))))

        XCTAssertGreaterThan(scan.opaque, 0, "nothing was rasterised — the scan proves nothing")
        XCTAssertEqual(scan.tinted, 0, "the OFF mark is tinted, so both modes look alike")
    }

    /// The claim the ON branch rests on: a non-template image is drawn exactly as
    /// authored, so the *gauge* half would freeze at one colour and be wrong in
    /// the other appearance — unless the dynamic colour is resolved inside the
    /// drawing handler, which runs at draw time.
    ///
    /// Asserted on one image drawn twice, not on two images: that is the stronger
    /// statement, and it is the one that fails if `NSImage` ever caches the first
    /// raster.
    func testTheGaugeReResolvesLabelColourPerAppearanceWhileTheTintHolds() throws {
        let image = try XCTUnwrap(
            MenuBarMark.image(gaugeSymbol: gauge, awake: true, awakeTint: loudTint))
        let light = try XCTUnwrap(rasterise(image, in: XCTUnwrap(NSAppearance(named: .aqua))))
        let dark = try XCTUnwrap(rasterise(image, in: XCTUnwrap(NSAppearance(named: .darkAqua))))

        XCTAssertGreaterThan(
            abs(light.untintedLuma - dark.untintedLuma), 0.2,
            "the gauge is the same brightness in both appearances, so labelColor was baked in")
        XCTAssertGreaterThan(light.tinted, 0, "the tint was lost in the light appearance")
        XCTAssertGreaterThan(dark.tinted, 0, "the tint was lost in the dark appearance")
    }

    /// `nil` rather than a blank image, so a caller notices. A missing SF Symbol
    /// is a fact about this build, not something to paper over with an empty
    /// status item.
    func testAMissingSymbolIsNilRatherThanABlankImage() {
        XCTAssertNil(
            MenuBarMark.image(
                gaugeSymbol: "not.a.real.sf.symbol.name", awake: false, awakeTint: loudTint))
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

        let image = try XCTUnwrap(
            MenuBarMark.image(gaugeSymbol: gauge, awake: true, awakeTint: loudTint))
        XCTAssertEqual(image.accessibilityDescription, on)
    }

    // MARK: - Rasterising

    private struct Scan {
        var opaque = 0
        /// Pixels that carry the tint — i.e. the cup.
        var tinted = 0
        /// Mean relative luminance of the opaque pixels that are NOT the tint —
        /// i.e. the gauge.
        var untintedLuma = 0.0
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
        var lumaSum = 0.0
        var lumaCount = 0
        for x in 0..<rep.pixelsWide {
            for y in 0..<rep.pixelsHigh {
                guard let colour = rep.colorAt(x: x, y: y)?.usingColorSpace(.sRGB),
                    colour.alphaComponent > 0.35
                else { continue }
                scan.opaque += 1
                let r = colour.redComponent
                let g = colour.greenComponent
                let b = colour.blueComponent
                // `loudTint` is pure red, so "carries the tint" is "much more red
                // than green or blue" — a predicate a grey or a white cannot
                // satisfy however bright it is.
                if r - g > 0.15 && r - b > 0.15 {
                    scan.tinted += 1
                } else {
                    lumaSum += 0.2126 * r + 0.7152 * g + 0.0722 * b
                    lumaCount += 1
                }
            }
        }
        scan.untintedLuma = lumaCount == 0 ? 0 : lumaSum / Double(lumaCount)
        return scan
    }
}
