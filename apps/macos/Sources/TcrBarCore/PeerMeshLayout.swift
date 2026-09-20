import CoreGraphics
import CoreText
import Foundation

// The mini mesh card at the top of the Peers tab.
//
// All of the geometry lives here, in `TcrBarCore`, and none of it in the view.
// The reason is the gate this card ships with: no pill may sit on top of
// another pill or on a tile, and that is a claim about rectangles. As data it
// is a test; inside a `Canvas` closure it would be a picture somebody has to
// squint at.

/// One Mac on the mini mesh, as the card needs it.
public struct PeerMeshPeer: Equatable, Sendable {
    public var name: String
    /// Round trip in milliseconds, or `nil` when nothing measured it.
    public var rttMs: Double?
    /// Fraction of probes lost, 0 to 1, or `nil` when unmeasured.
    public var lossPct: Double?
    /// The Mac carrying this one's bytes, when the path is forwarded.
    public var viaName: String?
    /// Away. A dimmed tile and NO pill: a sleeping Mac's last numbers are a
    /// stale reading, and this card does not draw those.
    public var asleep: Bool
    /// Whether the row this tile stands for currently has a path at all.
    /// Defaults `true` so a caller that has not been taught the real fact yet
    /// keeps drawing exactly what it drew before this field existed: a solid
    /// edge and a reading. `false` draws a dotted edge and a dotted `no path
    /// now` plate instead, never a reading: the row's own line one inch
    /// below already says there is nothing to report.
    public var hasPath: Bool
    /// WHICH absence, when there is one, in the row's own words.
    ///
    /// Read only when ``hasPath`` is `false`; a Mac with a path has nothing to
    /// name. The row has told these two apart since it grew the enum, and the
    /// card said `no path now` for both, so one screen answered "did this Mac
    /// look" two different ways an inch apart. Defaults to the case the card
    /// used to assume, so a caller that has not been taught the real fact yet
    /// draws exactly what it drew before this field existed.
    public var absence: PeerPathAbsence

    public init(
        name: String, rttMs: Double? = nil, lossPct: Double? = nil, viaName: String? = nil,
        asleep: Bool = false, hasPath: Bool = true, absence: PeerPathAbsence = .measured
    ) {
        self.name = name
        self.rttMs = rttMs
        self.lossPct = lossPct
        self.viaName = viaName
        self.asleep = asleep
        self.hasPath = hasPath
        self.absence = absence
    }
}

/// One line in the reading list drawn above ``PeerMeshLayout/maxMacsForGraph``
/// trusted Macs, where the card answers the same question a graph asks
/// without letting crossing edges and a collapsed tile stand in for it.
public struct PeerMeshReading: Equatable, Sendable {
    public var name: String
    /// `direct · 16 ms · 1% lost`, `via attic-nuc · 88 ms`, or `asleep · no
    /// path right now`.
    public var text: String
    /// Reads amber rather than the ordinary dim: a loss band above `ok`, or a
    /// round trip with no loss reading yet. Never set for a no-path row:
    /// there is nothing to warn about past "no path", which the text already
    /// says.
    public var warn: Bool

    public init(name: String, text: String, warn: Bool) {
        self.name = name
        self.text = text
        self.warn = warn
    }
}

/// How a path reads at a glance: the loss bands below, plus the
/// honest fourth for a path nobody has measured.
public enum PeerMeshTone: Equatable, Sendable {
    case ok
    case near
    case bad
    case unmeasured

    /// Green under 3 per cent, amber to 10, red above it, grey when the
    /// prober has not landed. One place, so the dot on a pill and the colour
    /// of its line cannot disagree.
    ///
    /// Grey answers one question only, "was loss measured", and a caller that
    /// also knows the round trip is known must not repaint that as "nothing
    /// measured": a known round trip with an unmeasured loss drops the pill's
    /// dot instead of reusing this tone for it (``PeerMeshLayout/Pill/showsToneDot``).
    /// Reserving grey that way is also why a sleeping Mac gets no pill at all
    /// rather than a grey one drawn from its last, stale numbers.
    public static func forLoss(_ lossPct: Double?) -> PeerMeshTone {
        guard let lossPct else { return .unmeasured }
        if lossPct < 0.03 { return .ok }
        if lossPct <= 0.10 { return .near }
        return .bad
    }
}

/// Everything the card draws, as rectangles and points.
public struct PeerMeshLayout: Equatable, Sendable {
    /// One tile: a Mac, the root, or the "+N" stand-in for the rest.
    public struct Node: Equatable, Sendable {
        public var label: String
        public var frame: CGRect
        /// The label sits under the tile and is part of what a pill must not
        /// land on, so it is carried rather than left to the view.
        public var labelFrame: CGRect
        public var isRoot: Bool
        public var asleep: Bool
        /// How many Macs this tile stands in for, on the collapsed tile only.
        public var collapsed: Int?

        public init(
            label: String, frame: CGRect, labelFrame: CGRect, isRoot: Bool = false,
            asleep: Bool = false, collapsed: Int? = nil
        ) {
            self.label = label
            self.frame = frame
            self.labelFrame = labelFrame
            self.isRoot = isRoot
            self.asleep = asleep
            self.collapsed = collapsed
        }
    }

    public struct Edge: Equatable, Sendable {
        public var from: CGPoint
        public var to: CGPoint
        /// Dashed because the bytes go through another Mac.
        public var carried: Bool
        /// Dashed because the row has no path at all right now. A separate
        /// fact from ``carried``: the two share one dash style (so the legend
        /// names every reason an edge dashes), but a
        /// no-path edge names no forwarder and its tone is always grey,
        /// which a carried edge's is not.
        public var noPath: Bool
        public var tone: PeerMeshTone

        public init(from: CGPoint, to: CGPoint, carried: Bool, noPath: Bool, tone: PeerMeshTone) {
            self.from = from
            self.to = to
            self.carried = carried
            self.noPath = noPath
            self.tone = tone
        }
    }

    /// One reading, on an opaque plate, over the edge it describes.
    public struct Pill: Equatable, Sendable {
        public var frame: CGRect
        /// `16 ms`, `not measured`, or one of the absence sentences
        /// (``PeerPathAbsence/plateSentence``).
        public var reading: String
        /// `via attic-nuc` on a carried path, and `nil` otherwise.
        public var via: String?
        public var tone: PeerMeshTone
        /// Whether the small tone dot draws at all. `false` for a known
        /// round trip with unmeasured loss: grey is reserved for a path
        /// nothing has measured, not for a gap in one measurement, and for
        /// a no-path plate, which carries no reading to colour.
        public var showsToneDot: Bool
        /// The plate's own border draws dashed on a no-path row, matching the
        /// edge it sits on.
        public var dashed: Bool

        public init(
            frame: CGRect, reading: String, via: String?, tone: PeerMeshTone,
            showsToneDot: Bool = true, dashed: Bool = false
        ) {
            self.frame = frame
            self.reading = reading
            self.via = via
            self.tone = tone
            self.showsToneDot = showsToneDot
            self.dashed = dashed
        }
    }

    public var size: CGSize
    public var nodes: [Node]
    public var edges: [Edge]
    public var pills: [Pill]
    /// Pills this layout could not place without landing on something else,
    /// counted rather than dropped in silence. The card says so, and the
    /// numbers are one click away behind Open full graph.
    public var unplacedReadings: Int

    /// The most Macs the card draws before the rest collapse into one tile.
    public static let visibleCap = 6

    /// Above this many trusted Macs the card stops drawing a graph at all:
    /// crossing edges, a pill sitting on its own line, one edge with no pill,
    /// and prints one reading per Mac instead
    /// (``PeerMeshReading``/``readings(for:)``). The collapse tile above
    /// ``visibleCap`` never has a chance to draw once this is past: the list
    /// has no cap of its own, every trusted Mac gets its own line.
    public static let maxMacsForGraph = 4

    /// The reading list drawn above ``maxMacsForGraph`` trusted Macs: one
    /// line per peer, in the words the graph's own pill and legend already
    /// use, so switching between the two views never teaches a second
    /// vocabulary for the same fact.
    public static func readings(for peers: [PeerMeshPeer]) -> [PeerMeshReading] {
        peers.map { peer in
            PeerMeshReading(name: peer.name, text: readingText(for: peer), warn: isWarnReading(peer))
        }
    }

    /// `direct · 16 ms · 1% lost`, `via attic-nuc · 88 ms`, or `asleep · no
    /// path right now`. Loss is only stated when it was measured: a `nil`
    /// loss says nothing, rather than printing a number that was never read,
    /// and a no-path row states only that, since a round trip or a loss
    /// figure would both claim something measured that was not.
    static func readingText(for peer: PeerMeshPeer) -> String {
        var parts: [String] = []
        if peer.asleep { parts.append("asleep") }
        if !peer.hasPath {
            // The row's own sentence, not a copy of it: the list and the row
            // are two printings of one fact, and the case that prints no line
            // at all prints none here either.
            if let sentence = peer.absence.sentence { parts.append(sentence) }
        } else {
            parts.append(peer.viaName.map { "via \($0)" } ?? "direct")
            if let rttMs = peer.rttMs { parts.append("\(Int(rttMs.rounded())) ms") }
            if let lossPct = peer.lossPct {
                parts.append(lossPct == 0 ? "no loss" : "\(Int((lossPct * 100).rounded()))% lost")
            }
        }
        return parts.joined(separator: " · ")
    }

    /// Whether a reading line reads amber: a loss band past `ok`, or a round
    /// trip whose loss was never read at all, the same "not a clean green"
    /// fact the graph's dropped dot states by omission, restated here since
    /// the list has no dot to drop. Never set for a sleeping or no-path row:
    /// there is nothing past "no path" to warn about.
    static func isWarnReading(_ peer: PeerMeshPeer) -> Bool {
        guard peer.hasPath, !peer.asleep else { return false }
        return PeerMeshTone.forLoss(peer.lossPct) != .ok
    }

    /// Lay one mesh out.
    ///
    /// Up to three Macs hang off the root in a single row; above that they
    /// take two rows, which is what keeps a 44 pt tile from having to shrink
    /// to fit an unbounded trust list. Past ``visibleCap`` the last tile
    /// becomes the "+N" stand-in, carrying no name and no reading, because it
    /// is not one peer and nothing it could say would be about one.
    public static func layout(
        size: CGSize, root: String, peers: [PeerMeshPeer]
    ) -> PeerMeshLayout {
        let tile = CGSize(width: 44, height: 36)
        let labelHeight: CGFloat = 14

        guard !peers.isEmpty else {
            return PeerMeshLayout(
                size: size, nodes: [], edges: [], pills: [], unplacedReadings: 0)
        }

        // The rows, and the collapse. `visibleCap` tiles at most, and when
        // more Macs than that are trusted the LAST of them is the stand-in.
        // The stand-in takes a tile of its own, so once there are more Macs
        // than tiles it stands in for everything past `visibleCap - 1`, not
        // just for the ones past the cap. Off by one here means a card that
        // says "and 1 more" over two missing Macs.
        let overflow = peers.count > visibleCap ? peers.count - (visibleCap - 1) : 0
        let drawn = overflow > 0 ? Array(peers.prefix(visibleCap - 1)) : peers
        let rows: [[PeerMeshPeer]]
        let stackRow: Int
        if drawn.count + (overflow > 0 ? 1 : 0) <= 3 {
            rows = [drawn]
            stackRow = 0
        } else {
            let firstCount = 2
            rows = [Array(drawn.prefix(firstCount)), Array(drawn.dropFirst(firstCount))]
            stackRow = 1
        }

        let rootFrame = CGRect(
            x: (size.width - tile.width) / 2, y: 18, width: tile.width, height: tile.height)
        var nodes: [Node] = [
            Node(
                label: root, frame: rootFrame,
                labelFrame: labelRect(under: rootFrame, height: labelHeight, width: size.width),
                isRoot: true)
        ]

        let bottomY = size.height - tile.height - labelHeight - 8
        let rowYs: [CGFloat] =
            rows.count == 1
            ? [bottomY]
            : [rootFrame.maxY + 44, bottomY]

        var peerNodes: [(peer: PeerMeshPeer, frame: CGRect)] = []
        for (rowIndex, row) in rows.enumerated() {
            let isStackRow = overflow > 0 && rowIndex == stackRow
            let count = row.count + (isStackRow ? 1 : 0)
            guard count > 0 else { continue }
            let step = size.width / CGFloat(count)
            for (columnIndex, peer) in row.enumerated() {
                let centreX = step * (CGFloat(columnIndex) + 0.5)
                let frame = CGRect(
                    x: centreX - tile.width / 2, y: rowYs[rowIndex],
                    width: tile.width, height: tile.height)
                peerNodes.append((peer, frame))
                nodes.append(
                    Node(
                        label: peer.name, frame: frame,
                        labelFrame: labelRect(
                            under: frame, height: labelHeight, width: size.width),
                        asleep: peer.asleep))
            }
            if isStackRow {
                let centreX = step * (CGFloat(count) - 0.5)
                let frame = CGRect(
                    x: centreX - tile.width / 2, y: rowYs[rowIndex],
                    width: tile.width, height: tile.height)
                nodes.append(
                    Node(
                        label: "and \(overflow) more", frame: frame,
                        labelFrame: labelRect(
                            under: frame, height: labelHeight, width: size.width),
                        collapsed: overflow))
            }
        }

        // Edges leave from BELOW the root's own name, not from the tile's
        // bottom edge. Drawn from the tile, two lines fanning out crossed the
        // name and made it unreadable, which is the same class of defect the
        // mockup's own first two passes shipped (a node name over an edge
        // label) and the reason every rectangle in this layout is data.
        let rootLabel = nodes[0].labelFrame
        let rootCentre = CGPoint(x: rootFrame.midX, y: rootLabel.maxY + 1)
        var edges: [Edge] = []
        var pills: [Pill] = []
        var taken: [CGRect] = nodes.map(\.frame) + nodes.map(\.labelFrame)
        var unplaced = 0

        for (peer, frame) in peerNodes {
            let to = CGPoint(x: frame.midX, y: frame.minY)
            let noPath = !peer.hasPath
            edges.append(
                Edge(
                    from: rootCentre, to: to, carried: peer.viaName != nil, noPath: noPath,
                    tone: noPath ? .unmeasured : PeerMeshTone.forLoss(peer.lossPct)))
            // A sleeping Mac gets no pill at all: its last numbers are a stale
            // reading and this card does not draw those.
            guard !peer.asleep else { continue }

            if noPath {
                // No path draws a dotted grey plate saying WHICH absence,
                // never a reading: a round trip or a loss figure both claim
                // something was measured, and nothing was. The words come off
                // the row's own enum, so the plate and the line under it
                // cannot answer "did this Mac look" differently.
                //
                // No plate at all for the absence that draws no line either:
                // there, traffic is flowing over a path the row cannot name,
                // and that is not an unplaced reading, so it is not counted as
                // one.
                guard let reading = peer.absence.plateSentence else { continue }
                let plate = plateSize(reading: reading, via: nil)
                guard
                    let frameForPill = place(
                        plate: plate, from: rootCentre, to: to, avoiding: taken, in: size)
                else {
                    unplaced += 1
                    continue
                }
                taken.append(frameForPill)
                pills.append(
                    Pill(
                        frame: frameForPill, reading: reading, via: nil, tone: .unmeasured,
                        showsToneDot: false, dashed: true))
                continue
            }

            let reading = peer.rttMs.map { "\(Int($0.rounded())) ms" } ?? "not measured"
            let via = peer.viaName.map { "via \($0)" }
            let plate = plateSize(reading: reading, via: via)
            guard
                let frameForPill = place(
                    plate: plate, from: rootCentre, to: to, avoiding: taken, in: size)
            else {
                unplaced += 1
                continue
            }
            taken.append(frameForPill)
            // A known round trip with unmeasured loss is not "nothing
            // measured": the dot drops instead of reusing grey for it.
            let showsDot = !(peer.rttMs != nil && peer.lossPct == nil)
            pills.append(
                Pill(
                    frame: frameForPill, reading: reading, via: via,
                    tone: PeerMeshTone.forLoss(peer.lossPct), showsToneDot: showsDot))
        }

        return PeerMeshLayout(
            size: size, nodes: nodes, edges: edges, pills: pills, unplacedReadings: unplaced)
    }

    // MARK: - How wide a reading's plate has to be

    /// The reading's own type size, and the forwarder line's under it. The
    /// card draws both at exactly these (`MiniMeshCard.pillLabel`), and the
    /// plate is measured at them, so the two cannot disagree about how much
    /// room a word needs.
    public static let pillReadingSize: CGFloat = 10
    public static let pillViaSize: CGFloat = 9
    /// What the plate holds beyond its text: the tone dot on the leading edge
    /// and the rounded border either side. The view narrows its label box by
    /// this same value, which is why it lives here rather than in the number
    /// sheet: a plate sized without it and a label drawn inside it would be
    /// two numbers for one gap.
    ///
    /// 24 and not 20. The view draws the label 8 pt right of the plate's
    /// centre to clear the dot, so a plate sized at text plus 20 left the last
    /// character 2 pt from its own border while the dot side had 6: the room
    /// was all spent on one edge. At `no path now` that read as tight; at
    /// `path not reported` the final `d` sits on the dashed border and the
    /// reading looks cut. 24 gives 4 pt past the text and 6 pt past the dot.
    public static let pillLabelInset: CGFloat = 24
    /// One line, and two when a forwarder is named.
    public static let pillOneLineHeight: CGFloat = 20
    public static let pillTwoLineHeight: CGFloat = 30

    /// A plate that fits what is written on it.
    ///
    /// # What a fixed width cost
    ///
    /// It was `via == nil ? 58 : 82`, regardless of content, and a reading
    /// has exactly two shapes: a round trip (`16 ms`) or the absence
    /// (`not measured`). The second needs about 65 pt at this size, so it
    /// rendered as `not m…` on EVERY unmeasured edge, in both appearances,
    /// and on the states where nothing has been probed yet that is every pill
    /// on screen. Naming an absence is this card's own discipline; truncating
    /// the name of it to four characters and an ellipsis is not.
    ///
    /// Measured with CoreText rather than estimated from a character count:
    /// the system font is proportional, so `1` and `m` are not one width, and
    /// a per-character guess is a second rendering engine that disagrees with
    /// the real one on some string nobody tried.
    public static func plateSize(reading: String, via: String?) -> CGSize {
        var width = textWidth(reading, size: pillReadingSize)
        if let via { width = max(width, textWidth(via, size: pillViaSize)) }
        return CGSize(
            width: (width + pillLabelInset).rounded(.up),
            height: via == nil ? pillOneLineHeight : pillTwoLineHeight)
    }

    /// One string's typographic width in the system font at `size`.
    ///
    /// CoreText and not AppKit: this module holds no views and links no UI
    /// framework, and `CTFontCreateUIFontForLanguage` answers with the same
    /// system font `Font.system(size:)` draws with.
    static func textWidth(_ text: String, size: CGFloat) -> CGFloat {
        guard let font = CTFontCreateUIFontForLanguage(.system, size, nil) else {
            // No silent fallback that pretends to be a measurement: an
            // en-width per character is stated as the estimate it is, and it
            // over-reserves rather than clipping. Not reachable on any macOS
            // this app runs on; the guard exists because the API is optional.
            return CGFloat(text.count) * size * 0.6
        }
        let line = CTLineCreateWithAttributedString(
            NSAttributedString(
                string: text, attributes: [kCTFontAttributeName as NSAttributedString.Key: font]))
        return CGFloat(CTLineGetTypographicBounds(line, nil, nil, nil))
    }

    /// The name under a tile, as wide as it may draw. Part of the layout
    /// because a pill landing on a name is the same defect as one landing on a
    /// tile, and the mockup's own first two passes shipped exactly that.
    private static func labelRect(under tile: CGRect, height: CGFloat, width: CGFloat)
        -> CGRect
    {
        let labelWidth: CGFloat = 74
        let x = min(max(0, tile.midX - labelWidth / 2), max(0, width - labelWidth))
        return CGRect(x: x, y: tile.maxY + 2, width: labelWidth, height: height)
    }

    /// Where one pill can sit on its own edge without landing on anything.
    ///
    /// The midpoint first, because that is where a reading belongs, then
    /// points along the SAME edge either side of it, then a small sideways
    /// nudge. Never a free-floating position: a pill that wandered off its
    /// line would describe an edge the reader cannot tell it belongs to.
    ///
    /// `nil` when nothing fits, and the caller counts that rather than
    /// stacking one reading on another.
    private static func place(
        plate: CGSize, from: CGPoint, to: CGPoint, avoiding taken: [CGRect], in size: CGSize
    ) -> CGRect? {
        // The midpoint first, then outward in both directions along the edge,
        // then sideways. Ordered by how far the candidate is from where the
        // reading belongs, so a crowded card degrades by moving readings a
        // little rather than by dropping one.
        let alongs: [CGFloat] = [
            0.5, 0.45, 0.55, 0.4, 0.6, 0.35, 0.65, 0.3, 0.7, 0.26, 0.74,
        ]
        let sideways: [CGFloat] = [0, -12, 12, -22, 22, -32, 32, -44, 44]
        let dx = to.x - from.x
        let dy = to.y - from.y
        let length = max(1, (dx * dx + dy * dy).squareRoot())
        // The unit normal to the edge, so a nudge moves the pill off the line
        // rather than along it twice.
        let normal = CGPoint(x: -dy / length, y: dx / length)

        for along in alongs {
            for offset in sideways {
                let centre = CGPoint(
                    x: from.x + dx * along + normal.x * offset,
                    y: from.y + dy * along + normal.y * offset)
                let candidate = CGRect(
                    x: centre.x - plate.width / 2, y: centre.y - plate.height / 2,
                    width: plate.width, height: plate.height)
                guard candidate.minX >= 0, candidate.minY >= 0,
                    candidate.maxX <= size.width, candidate.maxY <= size.height
                else { continue }
                if taken.contains(where: { $0.intersects(candidate) }) { continue }
                return candidate
            }
        }
        return nil
    }
}
