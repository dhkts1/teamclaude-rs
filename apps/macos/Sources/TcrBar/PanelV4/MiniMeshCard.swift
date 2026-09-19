import SwiftUI
import TcrBarCore

/// The mini mesh at the top of the Peers tab, Gil's topology-view direction.
///
/// This Mac in the middle of the top row with an accent ring, every trusted
/// Mac as a tile below it, one line per path coloured by loss and dashed when
/// the bytes are carried, and one opaque pill per line carrying the round trip
/// and, on a carried path, the Mac doing the carrying.
///
/// **Every rectangle here comes from ``PeerMeshLayout``**, which is in
/// `TcrBarCore` so the one rule this card must keep, no reading sitting on
/// another reading or on a tile, is a test over rectangles rather than a
/// picture somebody has to squint at. Both of the mockup's own rendering
/// passes shipped exactly that defect before a human caught it in a PNG.
struct MiniMeshCard: View {
    let root: String
    let peers: [PeerMeshPeer]
    /// Runs `tcr peer graph --serve` and opens the page it serves. Absent in
    /// the empty state: a link to a page with nothing on it is the phantom
    /// affordance this house style spends real effort avoiding.
    var onOpenGraph: () -> Void = {}

    /// Two heights, because four Macs need two rows of tiles and two do not.
    private var height: CGFloat { peers.count > 3 ? 300 : 206 }

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            if peers.isEmpty {
                Text("No trusted Macs yet.\nTrust one below to see it here.")
                    .font(V4.font(V4.muteSize))
                    .foregroundStyle(Tok.mute)
                    .multilineTextAlignment(.center)
                    .frame(maxWidth: .infinity, alignment: .center)
                    .padding(.vertical, V4.meshEmptyPaddingV)
            } else if peers.count > PeerMeshLayout.maxMacsForGraph {
                // Above the cap a graph stops answering its own question:
                // edges cross, a pill covers its own edge, one edge carries
                // none, so the card keeps the question and drops the
                // drawing: one reading per Mac, every one of them, no tile
                // standing in for the rest.
                readingsList

                Divider().overlay(Tok.cardLine)
                Button(action: onOpenGraph) {
                    HStack(spacing: 4) {
                        Text("Open full graph")
                        Image(systemName: "arrow.up.right")
                    }
                    .font(V4.font(V4.muteSize, .semibold))
                    .foregroundStyle(Tok.unmeasured)
                }
                .buttonStyle(.plain)
                .frame(maxWidth: .infinity, alignment: .trailing)
                .help("Serves the whole mesh as a page on this Mac and opens it.")
            } else {
                GeometryReader { proxy in
                    let layout = PeerMeshLayout.layout(
                        size: CGSize(width: proxy.size.width, height: height),
                        root: root, peers: peers)
                    ZStack(alignment: .topLeading) {
                        Canvas { context, _ in draw(layout, in: &context) }
                        ForEach(Array(layout.nodes.enumerated()), id: \.offset) { _, node in
                            tileGlyph(node)
                        }
                        ForEach(Array(layout.pills.enumerated()), id: \.offset) { _, pill in
                            pillLabel(pill)
                        }
                    }
                }
                .frame(height: height)
                .accessibilityElement(children: .ignore)
                .accessibilityLabel(spokenSummary)

                if layoutForCurrentWidth.unplacedReadings > 0 {
                    // Never a reading stacked on another reading: when the card
                    // runs out of room it says so and points at the page that
                    // has the numbers.
                    Text(
                        "Some readings do not fit here. Open full graph has all of them."
                    )
                    .font(V4.font(V4.muteSize))
                    .foregroundStyle(Tok.mute)
                }

                legend

                Divider().overlay(Tok.cardLine)
                Button(action: onOpenGraph) {
                    HStack(spacing: 4) {
                        Text("Open full graph")
                        Image(systemName: "arrow.up.right")
                    }
                    .font(V4.font(V4.muteSize, .semibold))
                    .foregroundStyle(Tok.unmeasured)
                }
                .buttonStyle(.plain)
                .frame(maxWidth: .infinity, alignment: .trailing)
                .help("Serves the whole mesh as a page on this Mac and opens it.")
            }
        }
        .padding(V4.meshCardPadding)
        .background(RoundedRectangle(cornerRadius: 8).fill(Tok.cardFill))
        .overlay(
            RoundedRectangle(cornerRadius: 8).strokeBorder(Tok.cardLine, lineWidth: 0.5)
        )
    }

    /// The same layout at the panel's own inner width, for the one question
    /// asked outside the `GeometryReader`: did every reading fit.
    private var layoutForCurrentWidth: PeerMeshLayout {
        PeerMeshLayout.layout(
            size: CGSize(width: V4.panelWidth - 36, height: height), root: root, peers: peers)
    }

    /// What decodes the drawing: solid against dashed, and grey against the
    /// three loss bands. Nothing else on the card says what a line style or a
    /// dot colour means.
    private var legend: some View {
        Text("solid: direct · dashed: carried by another Mac, or no path · grey: nothing measured")
            .font(.system(size: V4.meshLegendSize))
            .foregroundStyle(Tok.mute)
            .padding(.top, V4.meshLegendMarginTop)
    }

    /// Above ``PeerMeshLayout/maxMacsForGraph`` trusted Macs: the same facts
    /// the graph draws, one line per Mac, in the order they were trusted.
    private var readingsList: some View {
        VStack(alignment: .leading, spacing: V4.meshReadingsGap) {
            ForEach(PeerMeshLayout.readings(for: peers), id: \.name) { reading in
                HStack(spacing: V4.rowGap) {
                    Text(reading.name)
                        .font(.system(size: V4.meshReadingNameSize))
                        .foregroundStyle(Tok.dim)
                    Spacer(minLength: 0)
                    Text(reading.text)
                        .font(.system(size: V4.meshReadingTextSize))
                        .foregroundStyle(reading.warn ? Tok.near : Tok.mute)
                }
            }
        }
        .padding(.top, V4.meshReadingsMarginTop)
    }

    /// What VoiceOver reads instead of a drawing. The same facts the pills
    /// carry: a canvas with no spoken form is a picture of information nobody
    /// can hear.
    private var spokenSummary: String {
        let parts = peers.map { peer -> String in
            if peer.asleep { return "\(peer.name) is away" }
            let rtt = peer.rttMs.map { "\(Int($0.rounded())) milliseconds" } ?? "not measured"
            let via = peer.viaName.map { ", carried by \($0)" } ?? ""
            return "\(peer.name) \(rtt)\(via)"
        }
        return "Mesh around \(root). " + parts.joined(separator: ". ")
    }

    private func draw(_ layout: PeerMeshLayout, in context: inout GraphicsContext) {
        for edge in layout.edges {
            var path = Path()
            path.move(to: edge.from)
            path.addLine(to: edge.to)
            context.stroke(
                path, with: .color(tint(edge.tone)),
                style: StrokeStyle(
                    lineWidth: 2, lineCap: .round,
                    dash: (edge.carried || edge.noPath) ? [4, 3.5] : []))
        }
        for node in layout.nodes {
            let tile = Path(roundedRect: node.frame, cornerRadius: 8)
            context.fill(tile, with: .color(Tok.panel))
            context.stroke(
                tile, with: .color(node.isRoot ? Tok.unmeasured : Tok.hairlineStrong),
                style: StrokeStyle(
                    lineWidth: node.isRoot ? 1.8 : 1.4,
                    dash: node.collapsed == nil ? [] : [2.5, 2.5]))
            if node.isRoot {
                // The accent ring: this Mac, and no label needed to say so.
                context.stroke(
                    Path(roundedRect: node.frame.insetBy(dx: -3, dy: -3), cornerRadius: 10),
                    with: .color(Tok.unmeasured.opacity(0.55)), lineWidth: 1.4)
            }
        }
        for pill in layout.pills {
            let plate = Path(roundedRect: pill.frame, cornerRadius: 7)
            // OPAQUE, so no line can ever show through a reading.
            context.fill(plate, with: .color(Tok.cardFill))
            context.stroke(
                plate, with: .color(Tok.hairlineStrong),
                style: StrokeStyle(lineWidth: 1, dash: pill.dashed ? [3, 3] : []))
            // Grey is reserved for "nothing measured": a known round trip
            // with unmeasured loss, and a no-path plate, draw no dot at all
            // rather than reusing it.
            if pill.showsToneDot {
                let dot = CGRect(
                    x: pill.frame.minX + 8, y: pill.frame.minY + (pill.via == nil ? 7 : 8),
                    width: 6, height: 6)
                context.fill(Path(ellipseIn: dot), with: .color(tint(pill.tone)))
            }
        }
    }

    /// The laptop glyph and the name, drawn as views over the canvas: `Canvas`
    /// can draw text, and a `Text` here keeps the app's own font and its
    /// dynamic-type behaviour rather than a second type stack inside a
    /// drawing closure.
    @ViewBuilder
    private func tileGlyph(_ node: PeerMeshLayout.Node) -> some View {
        Group {
            if let collapsed = node.collapsed {
                Text("+\(collapsed)")
                    .font(V4.font(V4.nameSize, .semibold))
                    .foregroundStyle(Tok.mute)
                    .frame(width: node.frame.width, height: node.frame.height)
                    .position(x: node.frame.midX, y: node.frame.midY)
            } else {
                Image(systemName: "laptopcomputer")
                    .font(.system(size: 17, weight: .regular))
                    .foregroundStyle(node.isRoot ? Tok.ink : Tok.dim)
                    .opacity(node.asleep ? 0.48 : 1)
                    .position(x: node.frame.midX, y: node.frame.midY)
            }
            // The glyph dims to say "away"; the name stays at full ink. It is
            // how a person identifies the row they are about to act on, and
            // it was the faintest text on the panel before this.
            Text(node.label)
                .font(V4.font(V4.muteSize))
                .foregroundStyle(node.isRoot ? Tok.ink : Tok.dim)
                .lineLimit(1)
                .frame(width: node.labelFrame.width)
                .position(x: node.labelFrame.midX, y: node.labelFrame.midY)
        }
    }

    private func pillLabel(_ pill: PeerMeshLayout.Pill) -> some View {
        VStack(alignment: .leading, spacing: 1) {
            // The sizes the plate was MEASURED at. Written here as literals
            // they were a second copy of the two numbers that decide whether
            // the text fits the box it is drawn in.
            Text(pill.reading)
                .font(.system(size: PeerMeshLayout.pillReadingSize))
                // A no-path plate names an absence, not a measurement: it
                // reads in the same mute ink the legend and the row below it
                // already use for "nothing measured", not the bright ink a
                // real reading gets.
                .foregroundStyle(pill.dashed ? Tok.mute : Tok.ink)
            if let via = pill.via {
                Text(via)
                    .font(.system(size: PeerMeshLayout.pillViaSize))
                    .foregroundStyle(Tok.mute)
            }
        }
        .lineLimit(1)
        .frame(width: pill.frame.width - V4.meshPillLabelInset, alignment: .leading)
        .position(x: pill.frame.midX + 8, y: pill.frame.midY)
    }

    private func tint(_ tone: PeerMeshTone) -> Color {
        switch tone {
        case .ok: return Tok.ok
        case .near: return Tok.near
        case .bad: return Tok.spent
        case .unmeasured: return Tok.disabled
        }
    }
}
