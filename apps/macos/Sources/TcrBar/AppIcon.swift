import AppKit
import Foundation

/// The app mark, drawn in code.
///
/// ## Why code and not an asset
///
/// A committed `.icns` is a binary that drifts from the palette silently: change a
/// token and the icon keeps the old colour, and nothing fails. Drawing it from the
/// same `Tok` values means the mark cannot disagree with the app, and the whole
/// icon set regenerates from one command.
///
/// ## What it means
///
/// An almost-closed ring with a single bright dot at its leading end. The ring is
/// the pool of accounts and the gap is the one being spent; the dot is the account
/// currently serving, and its position is where rotation has reached. It reads as
/// rotation and as progress, which is what this proxy does.
///
/// Deliberately ONE mark with ONE accent. At 16pt — the size that actually matters,
/// because that is the Finder list and the Login Items row — anything with more
/// than two elements turns to mush. The design was checked at 16pt first and scaled
/// up, not the reverse.
enum AppIcon {

    /// Draw the mark at `size` points, square.
    static func image(size: CGFloat) -> NSImage {
        NSImage(size: NSSize(width: size, height: size), flipped: false) { rect in
            let s = size / 1024.0  // design space is 1024, the largest icns slot

            // macOS icons do not fill their square: the grid leaves a margin so
            // adjacent icons breathe. ~10% inset matches the system look.
            let ground = rect.insetBy(dx: 100 * s, dy: 100 * s)

            // Warm near-black, matching the panel's darkest surface rather than
            // pure black, which reads as a hole next to real macOS icons.
            let squircle = NSBezierPath(
                roundedRect: ground, xRadius: 185 * s, yRadius: 185 * s)
            NSColor(srgbRed: 0x09 / 255, green: 0x0c / 255, blue: 0x10 / 255, alpha: 1).setFill()
            squircle.fill()

            // A hairline lip so the mark still has an edge on a dark wallpaper.
            NSColor(srgbRed: 0x53 / 255, green: 0x59 / 255, blue: 0x60 / 255, alpha: 1).setStroke()
            squircle.lineWidth = 6 * s
            squircle.stroke()

            let centre = NSPoint(x: rect.midX, y: rect.midY)
            let radius = 250 * s
            let ringWidth = 84 * s

            // The pool: an open ring. The gap is deliberate — a closed circle reads
            // as a status light, an open one reads as rotation with somewhere left
            // to go.
            let ring = NSBezierPath()
            ring.appendArc(
                withCenter: centre, radius: radius, startAngle: 118, endAngle: 62,
                clockwise: false)
            ring.lineWidth = ringWidth
            ring.lineCapStyle = .round
            NSColor(srgbRed: 0x39 / 255, green: 0x3e / 255, blue: 0x43 / 255, alpha: 1).setStroke()
            ring.stroke()

            // The served portion, in the ready green. Stops short of the dot so the
            // two read as one motion rather than a single blob.
            let served = NSBezierPath()
            served.appendArc(
                withCenter: centre, radius: radius, startAngle: 118, endAngle: 300,
                clockwise: true)
            served.lineWidth = ringWidth
            served.lineCapStyle = .round
            NSColor(srgbRed: 0x66 / 255, green: 0xd0 / 255, blue: 0x81 / 255, alpha: 1).setStroke()
            served.stroke()

            // The live account: one bright dot at the leading edge.
            let angle = 300.0 * .pi / 180.0
            let dot = NSPoint(
                x: centre.x + radius * cos(angle), y: centre.y + radius * sin(angle))
            let dotRadius = 86 * s
            let dotPath = NSBezierPath(
                ovalIn: NSRect(
                    x: dot.x - dotRadius, y: dot.y - dotRadius,
                    width: dotRadius * 2, height: dotRadius * 2))
            NSColor(srgbRed: 0xf2 / 255, green: 0xf2 / 255, blue: 0xef / 255, alpha: 1).setFill()
            dotPath.fill()

            return true
        }
    }

    /// Every slot `iconutil` expects. Missing one does not fail the build — it just
    /// makes macOS scale a neighbour, which is where a soft, slightly wrong icon in
    /// the Dock comes from.
    private static let slots: [(name: String, points: CGFloat, scale: CGFloat)] = [
        ("icon_16x16", 16, 1), ("icon_16x16@2x", 16, 2),
        ("icon_32x32", 32, 1), ("icon_32x32@2x", 32, 2),
        ("icon_128x128", 128, 1), ("icon_128x128@2x", 128, 2),
        ("icon_256x256", 256, 1), ("icon_256x256@2x", 256, 2),
        ("icon_512x512", 512, 1), ("icon_512x512@2x", 512, 2),
    ]

    static let flag = "--render-icon"

    static func requestedDirectory(_ arguments: [String] = CommandLine.arguments) -> URL? {
        guard let i = arguments.firstIndex(of: flag), i + 1 < arguments.count else { return nil }
        return URL(fileURLWithPath: arguments[i + 1])
    }

    /// Write a full `.iconset` directory, which `iconutil -c icns` turns into the
    /// bundle's icon.
    static func writeIconSet(to directory: URL) -> Never {
        do {
            try FileManager.default.createDirectory(
                at: directory, withIntermediateDirectories: true)
        } catch {
            FileHandle.standardError.write(Data("cannot create \(directory.path)\n".utf8))
            exit(1)
        }

        var written = 0
        for slot in slots {
            let pixels = slot.points * slot.scale
            let image = image(size: pixels)
            guard let tiff = image.tiffRepresentation,
                let rep = NSBitmapImageRep(data: tiff),
                let png = rep.representation(using: .png, properties: [:])
            else {
                FileHandle.standardError.write(Data("render failed: \(slot.name)\n".utf8))
                continue
            }
            let url = directory.appendingPathComponent("\(slot.name).png")
            do {
                try png.write(to: url)
                written += 1
            } catch {
                FileHandle.standardError.write(Data("write failed: \(slot.name)\n".utf8))
            }
        }
        print("wrote \(written)/\(slots.count) icon slots into \(directory.path)")
        exit(written == slots.count ? 0 : 1)
    }
}
