import XCTest

@testable import TcrBarCore

/// The mini mesh card's geometry, and the gate it ships with.
final class PeerMeshLayoutTests: XCTestCase {
    /// The card's real drawing area inside the 372 pt panel.
    private let small = CGSize(width: 340, height: 206)
    private let tall = CGSize(width: 340, height: 300)

    private var twoMacs: [PeerMeshPeer] {
        [
            PeerMeshPeer(name: "attic-nuc", rttMs: 16, lossPct: 0.01),
            PeerMeshPeer(name: "loft-mini", rttMs: 58, lossPct: 0.05, viaName: "attic-nuc"),
        ]
    }

    /// The scene the brief names as the gate: seven trusted Macs, five drawn,
    /// two collapsed, every loss band and both line styles at once.
    private var sevenMacs: [PeerMeshPeer] {
        [
            PeerMeshPeer(name: "attic-nuc", rttMs: 16, lossPct: 0.01),
            PeerMeshPeer(name: "loft-mini", rttMs: 72, lossPct: 0.06, viaName: "attic-nuc"),
            PeerMeshPeer(name: "office-mini", rttMs: 210, lossPct: 0.14),
            PeerMeshPeer(name: "lab-mac", asleep: true, hasPath: false),
            PeerMeshPeer(name: "gil-laptop", rttMs: 88, viaName: "attic-nuc"),
            PeerMeshPeer(name: "shed-mac", rttMs: 24, lossPct: 0),
            PeerMeshPeer(name: "van-mac", rttMs: 31, lossPct: 0),
        ]
    }

    // MARK: THE gate

    /// No pill may sit on another pill, on a tile, or on a tile's name. Both
    /// of the mockup's own rendering passes shipped exactly this defect
    /// ("attic-nuc" over "16 ms"), and it is a claim about rectangles, so it
    /// is checked as one rather than looked at.
    func testNoPillIntersectsAnotherPillATileOrAName() {
        let layout = PeerMeshLayout.layout(size: tall, root: "desk-mac", peers: sevenMacs)
        XCTAssertFalse(layout.pills.isEmpty, "a fixture with no pills would pass vacuously")
        let tiles = layout.nodes.map(\.frame) + layout.nodes.map(\.labelFrame)
        for (index, pill) in layout.pills.enumerated() {
            for tile in tiles {
                XCTAssertFalse(
                    pill.frame.intersects(tile),
                    "pill \(pill.reading) lands on a tile or a name at \(pill.frame)")
            }
            for other in layout.pills.dropFirst(index + 1) {
                XCTAssertFalse(
                    pill.frame.intersects(other.frame),
                    "pill \(pill.reading) lands on pill \(other.reading)")
            }
        }
    }

    /// And the same for the two-Mac card, which is the one an operator with a
    /// small mesh actually looks at every day.
    func testTheTwoMacCardIsAlsoFreeOfCollisions() {
        let layout = PeerMeshLayout.layout(size: small, root: "desk-mac", peers: twoMacs)
        let tiles = layout.nodes.map(\.frame) + layout.nodes.map(\.labelFrame)
        XCTAssertEqual(layout.pills.count, 2)
        for (index, pill) in layout.pills.enumerated() {
            for tile in tiles { XCTAssertFalse(pill.frame.intersects(tile)) }
            for other in layout.pills.dropFirst(index + 1) {
                XCTAssertFalse(pill.frame.intersects(other.frame))
            }
        }
    }

    /// Every pill is inside the card. A reading half off the edge is the same
    /// failure as one under a tile: the operator cannot read it.
    func testEveryPillIsInsideTheCard() {
        let layout = PeerMeshLayout.layout(size: tall, root: "desk-mac", peers: sevenMacs)
        let bounds = CGRect(origin: .zero, size: tall)
        for pill in layout.pills {
            XCTAssertTrue(bounds.contains(pill.frame), "\(pill.reading) at \(pill.frame)")
        }
    }

    // MARK: The states

    func testTheEmptyMeshDrawsNothingAtAll() {
        let layout = PeerMeshLayout.layout(size: small, root: "desk-mac", peers: [])
        XCTAssertTrue(layout.nodes.isEmpty, "no ring, and no lone root floating in it")
        XCTAssertTrue(layout.edges.isEmpty)
        XCTAssertTrue(layout.pills.isEmpty)
    }

    func testTheRootCarriesItsOwnTileAndEveryPeerGetsOne() {
        let layout = PeerMeshLayout.layout(size: small, root: "desk-mac", peers: twoMacs)
        XCTAssertEqual(layout.nodes.count, 3)
        XCTAssertEqual(layout.nodes.first?.isRoot, true)
        XCTAssertEqual(layout.nodes.first?.label, "desk-mac")
        XCTAssertEqual(layout.edges.count, 2)
    }

    /// A carried path is dashed and names its forwarder on the pill's second
    /// line; a direct one does neither.
    func testACarriedPathIsDashedAndNamesItsForwarder() {
        let layout = PeerMeshLayout.layout(size: small, root: "desk-mac", peers: twoMacs)
        XCTAssertEqual(layout.edges.map(\.carried), [false, true])
        XCTAssertEqual(layout.pills.map(\.via), [nil, "via attic-nuc"])
        XCTAssertEqual(layout.pills.map(\.reading), ["16 ms", "58 ms"])
    }

    /// A sleeping Mac keeps its tile, dimmed, and gets NO pill: its last
    /// numbers are a stale reading and this card does not draw those.
    func testASleepingMacHasATileAndNoPill() {
        let layout = PeerMeshLayout.layout(size: tall, root: "desk-mac", peers: sevenMacs)
        let sleeping = layout.nodes.first { $0.label == "lab-mac" }
        XCTAssertEqual(sleeping?.asleep, true)
        XCTAssertFalse(layout.pills.contains { $0.reading.contains("lab-mac") })
        // Five drawn Macs, one of them asleep, so four readings.
        XCTAssertEqual(layout.pills.count, 4)
        XCTAssertEqual(layout.unplacedReadings, 0)
    }

    /// Seven trusted, five drawn, and one tile standing in for the rest. It
    /// carries no name and no reading, because it is not one peer.
    func testPastTheCapTheRestCollapseIntoOneTile() {
        let layout = PeerMeshLayout.layout(size: tall, root: "desk-mac", peers: sevenMacs)
        let stack = layout.nodes.first { $0.collapsed != nil }
        XCTAssertEqual(stack?.collapsed, 2)
        XCTAssertEqual(stack?.label, "and 2 more")
        // Root, five Macs, one stand-in.
        XCTAssertEqual(layout.nodes.count, 7)
        // No edge to the stand-in: it is a count, not a path.
        XCTAssertEqual(layout.edges.count, 5)
    }

    func testExactlySixTrustedMacsAreAllDrawnWithNoStandIn() {
        let six = Array(sevenMacs.prefix(6))
        let layout = PeerMeshLayout.layout(size: tall, root: "desk-mac", peers: six)
        XCTAssertNil(layout.nodes.first { $0.collapsed != nil })
        XCTAssertEqual(layout.nodes.count, 7)
        XCTAssertEqual(layout.edges.count, 6)
    }

    /// Up to three Macs sit in one row; above that the card takes two, which
    /// is what keeps a tile from having to shrink.
    func testThreeMacsShareOneRowAndFourTakeTwo() {
        let three = Array(sevenMacs.prefix(3))
        let flat = PeerMeshLayout.layout(size: small, root: "desk-mac", peers: three)
        let rowsInFlat = Set(flat.nodes.dropFirst().map(\.frame.minY))
        XCTAssertEqual(rowsInFlat.count, 1)

        let four = Array(sevenMacs.prefix(4))
        let tree = PeerMeshLayout.layout(size: tall, root: "desk-mac", peers: four)
        let rowsInTree = Set(tree.nodes.dropFirst().map(\.frame.minY))
        XCTAssertEqual(rowsInTree.count, 2)
    }

    // MARK: Argv

    /// The card's one control. `--serve` is what makes the verb a page rather
    /// than a print, and it binds loopback only.
    func testOpenFullGraphRunsTheServingVerb() {
        XCTAssertEqual(PeerCommand.graphServe, ["peer", "graph", "--serve"])
    }

    // MARK: The colour rule

    /// Green under 3 per cent, amber to 10, red above it, and grey for a path
    /// nothing has measured, which is a different fact from a healthy one.
    func testTheLossBandsAreTheOnesThisWaveSets() {
        XCTAssertEqual(PeerMeshTone.forLoss(0), .ok)
        XCTAssertEqual(PeerMeshTone.forLoss(0.029), .ok)
        XCTAssertEqual(PeerMeshTone.forLoss(0.03), .near)
        XCTAssertEqual(PeerMeshTone.forLoss(0.10), .near)
        XCTAssertEqual(PeerMeshTone.forLoss(0.11), .bad)
        XCTAssertEqual(PeerMeshTone.forLoss(nil), .unmeasured)
    }

    /// An unmeasured path says so on its pill rather than printing a zero, and
    /// a carried unmeasured path still names its forwarder: "not measured" and
    /// "carried" are two independent facts.
    func testAnUnmeasuredCarriedPathSaysBoth() {
        let layout = PeerMeshLayout.layout(
            size: small, root: "desk-mac",
            peers: [PeerMeshPeer(name: "gil-laptop", viaName: "attic-nuc")])
        XCTAssertEqual(layout.pills.first?.reading, "not measured")
        XCTAssertEqual(layout.pills.first?.via, "via attic-nuc")
        XCTAssertEqual(layout.pills.first?.tone, .unmeasured)
        XCTAssertEqual(layout.edges.first?.carried, true)
    }

    /// Every plate holds the words written on it.
    ///
    /// The width was fixed at 58 pt whatever the reading said, and a reading
    /// has two shapes: a round trip, or the absence. `not measured` needs
    /// about 65 pt at this size, so the absence printed `not m…` on every
    /// unmeasured edge in both appearances, and on a fresh network, where
    /// nothing has been probed yet, that is every pill on the card.
    func testAPlateIsWideEnoughForWhatIsWrittenOnIt() {
        for reading in ["not measured", "16 ms", "210 ms", "8 ms"] {
            let plate = PeerMeshLayout.plateSize(reading: reading, via: nil)
            let needed =
                PeerMeshLayout.textWidth(reading, size: PeerMeshLayout.pillReadingSize)
                + PeerMeshLayout.pillLabelInset
            XCTAssertGreaterThanOrEqual(
                plate.width, needed,
                "the plate for \(reading) is narrower than the text it holds, so the reading "
                    + "renders with an ellipsis where its last characters belong")
        }
        XCTAssertGreaterThan(
            PeerMeshLayout.plateSize(reading: "not measured", via: nil).width,
            PeerMeshLayout.plateSize(reading: "16 ms", via: nil).width,
            "both readings get one width again, so one of the two must truncate")
    }

    /// A forwarder line is measured too, and it is the longer of the two.
    func testACarriedPlateFitsItsForwarderLine() {
        let plate = PeerMeshLayout.plateSize(
            reading: "not measured", via: "via a-very-long-machine-name")
        let needed =
            PeerMeshLayout.textWidth("via a-very-long-machine-name", size: PeerMeshLayout.pillViaSize)
            + PeerMeshLayout.pillLabelInset
        XCTAssertGreaterThanOrEqual(
            plate.width, needed,
            "the forwarder's name is clipped, which is the one word that says WHICH Mac is "
                + "carrying the bytes")
        XCTAssertEqual(plate.height, PeerMeshLayout.pillTwoLineHeight)
    }

    // MARK: No path

    /// A row with no path draws a dotted grey edge and a dotted `no path
    /// now` plate, never a reading, because a round trip or a loss figure
    /// would both claim something was measured that was not.
    func testNoPathDrawsADottedGreyEdgeAndPlateNeverAReading() {
        let layout = PeerMeshLayout.layout(
            size: small, root: "desk-mac",
            peers: [PeerMeshPeer(name: "office-mini", rttMs: 210, lossPct: 0.14, hasPath: false)])
        XCTAssertEqual(layout.edges.first?.noPath, true)
        XCTAssertEqual(layout.edges.first?.tone, .unmeasured)
        XCTAssertEqual(layout.pills.first?.reading, "no path now")
        XCTAssertEqual(layout.pills.first?.via, nil)
        XCTAssertEqual(layout.pills.first?.dashed, true)
        XCTAssertEqual(layout.pills.first?.showsToneDot, false)
    }

    /// The plate says WHICH absence, in the words the row under it uses.
    ///
    /// The row has told a measured absence from an unreported one since it
    /// grew the enum; the card printed `no path now` over both, so one screen
    /// answered "did this Mac look" two different ways an inch apart, and the
    /// plate was the half making a claim it had not measured.
    func testThePlateNamesWhichAbsenceTheRowNamed() {
        let reported = PeerMeshLayout.layout(
            size: small, root: "desk-mac",
            peers: [PeerMeshPeer(name: "office-mini", hasPath: false, absence: .notReported)])
        XCTAssertEqual(reported.pills.first?.reading, "path not reported")
        XCTAssertEqual(reported.pills.first?.dashed, true)

        let measured = PeerMeshLayout.layout(
            size: small, root: "desk-mac",
            peers: [PeerMeshPeer(name: "office-mini", hasPath: false, absence: .measured)])
        XCTAssertEqual(measured.pills.first?.reading, "no path now")
    }

    /// The absence that draws no line draws no plate either, and that is not
    /// a reading the card failed to place.
    ///
    /// Work is flowing over a path the row cannot name, which is why the row
    /// prints nothing; a plate saying "no path now" over the same tile is the
    /// card contradicting the row two inches below it.
    func testTheSilentAbsenceDrawsNoPlateAndIsNotCountedUnplaced() {
        let layout = PeerMeshLayout.layout(
            size: small, root: "desk-mac",
            peers: [PeerMeshPeer(name: "office-mini", hasPath: false, absence: .silent)])
        XCTAssertEqual(layout.pills.count, 0)
        XCTAssertEqual(layout.unplacedReadings, 0)
        XCTAssertEqual(layout.edges.first?.noPath, true, "the edge still says there is no path")
    }

    /// The reading list above the graph cap names the absence too, in the
    /// row's own longer sentence, since a list line has the room a plate does
    /// not.
    func testTheReadingsListNamesWhichAbsence() {
        let readings = PeerMeshLayout.readings(for: [
            PeerMeshPeer(name: "office-mini", hasPath: false, absence: .notReported),
            PeerMeshPeer(name: "loft-mini", hasPath: false, absence: .measured),
            PeerMeshPeer(name: "shed-mac", hasPath: false, absence: .silent),
        ])
        XCTAssertEqual(readings.map(\.text), ["path not reported", "no path right now", ""])
    }

    /// The widest plate the card can be asked to draw still gets placed at the
    /// panel's own inner width.
    ///
    /// `path not reported` is half as wide again as `no path now`, and a plate
    /// the placer cannot fit is a reading the card drops into its
    /// "do not fit here" footer. Measured at the width `MiniMeshCard` lays the
    /// card out at, 372 pt of panel less its 36 pt of gutters, on the shape
    /// that crowds a plate most: two Macs, so both edges lean.
    func testTheWidestAbsencePlateIsPlacedAtThePanelsOwnWidth() {
        let cardWidth = CGSize(width: 336, height: 206)
        let layout = PeerMeshLayout.layout(
            size: cardWidth, root: "desk-mac",
            peers: [
                PeerMeshPeer(name: "attic-nuc", rttMs: 16, lossPct: 0.01),
                PeerMeshPeer(name: "loft-mini", hasPath: false, absence: .notReported),
            ])
        XCTAssertEqual(layout.unplacedReadings, 0, "the widest absence plate had nowhere to go")
        XCTAssertEqual(layout.pills.count, 2)
        for pill in layout.pills {
            XCTAssertGreaterThanOrEqual(pill.frame.minX, 0)
            XCTAssertLessThanOrEqual(
                pill.frame.maxX, cardWidth.width,
                "the plate runs off the right edge of the card, which is where the reading is cut")
        }
        let plate = PeerMeshLayout.plateSize(reading: "path not reported", via: nil)
        XCTAssertGreaterThanOrEqual(
            plate.width,
            PeerMeshLayout.textWidth("path not reported", size: PeerMeshLayout.pillReadingSize)
                + PeerMeshLayout.pillLabelInset,
            "the plate is narrower than the sentence on it")
    }

    /// A row that does have a path draws neither dotted.
    func testAPathDrawsNeitherEdgeNorPlateDashed() {
        let layout = PeerMeshLayout.layout(
            size: small, root: "desk-mac", peers: [PeerMeshPeer(name: "attic-nuc", rttMs: 16)])
        XCTAssertEqual(layout.edges.first?.noPath, false)
        XCTAssertEqual(layout.pills.first?.dashed, false)
    }

    // MARK: Grey is reserved for nothing measured

    /// A known round trip with unmeasured loss is not "nothing measured": the
    /// dot drops rather than reusing grey for it.
    func testAKnownRoundTripWithUnmeasuredLossDropsTheDot() {
        let layout = PeerMeshLayout.layout(
            size: small, root: "desk-mac", peers: [PeerMeshPeer(name: "gil-laptop", rttMs: 88)])
        XCTAssertEqual(layout.pills.first?.showsToneDot, false)
    }

    /// Nothing measured at all, no round trip, no loss, keeps the grey dot.
    func testGreyDotStaysWhenNothingAtAllWasMeasured() {
        let layout = PeerMeshLayout.layout(
            size: small, root: "desk-mac",
            peers: [PeerMeshPeer(name: "gil-laptop", viaName: "attic-nuc")])
        XCTAssertEqual(layout.pills.first?.showsToneDot, true)
        XCTAssertEqual(layout.pills.first?.tone, .unmeasured)
    }

    /// A known round trip WITH a known loss keeps the dot too: this is not a
    /// blanket "carried paths never dot" rule.
    func testAFullyMeasuredPathKeepsTheDot() {
        let layout = PeerMeshLayout.layout(
            size: small, root: "desk-mac",
            peers: [PeerMeshPeer(name: "attic-nuc", rttMs: 16, lossPct: 0.01)])
        XCTAssertEqual(layout.pills.first?.showsToneDot, true)
    }

    // MARK: Above the cap, one reading per Mac

    func testAboveFourTrustedMacsIsTheNamedThreshold() {
        XCTAssertEqual(PeerMeshLayout.maxMacsForGraph, 4)
    }

    /// The list states the same facts the graph's pill and edge would, in
    /// the graph's own words, for every trusted Mac, no cap, no stand-in.
    func testReadingsListOneLinePerMacInTheGraphsOwnWords() {
        let readings = PeerMeshLayout.readings(for: sevenMacs)
        XCTAssertEqual(readings.count, 7, "no collapse tile: every trusted Mac gets a line")
        XCTAssertEqual(
            readings.map(\.text),
            [
                "direct · 16 ms · 1% lost",
                "via attic-nuc · 72 ms · 6% lost",
                "direct · 210 ms · 14% lost",
                "asleep · no path right now",
                "via attic-nuc · 88 ms",
                "direct · 24 ms · no loss",
                "direct · 31 ms · no loss",
            ])
        XCTAssertEqual(readings.map(\.name), sevenMacs.map(\.name))
    }

    /// Warn is a loss band past `ok`, or a round trip whose loss was never
    /// read, never set for the asleep, no-path row.
    func testReadingsFlagWarnForAnythingNotACleanGreen() {
        let readings = PeerMeshLayout.readings(for: sevenMacs)
        XCTAssertEqual(
            Dictionary(uniqueKeysWithValues: readings.map { ($0.name, $0.warn) }),
            [
                "attic-nuc": false,
                "loft-mini": true,
                "office-mini": true,
                "lab-mac": false,
                "gil-laptop": true,
                "shed-mac": false,
                "van-mac": false,
            ])
    }

    /// The measurement is a real one. A positive control, because a text
    /// measurer that answered zero would make every assertion above pass by
    /// reporting that nothing needs any room.
    func testTheTextMeasurerMeasures() {
        let short = PeerMeshLayout.textWidth("8 ms", size: PeerMeshLayout.pillReadingSize)
        let long = PeerMeshLayout.textWidth("not measured", size: PeerMeshLayout.pillReadingSize)
        XCTAssertGreaterThan(short, 0, "the measurer answers zero, so every plate fits by default")
        XCTAssertGreaterThan(long, short)
        XCTAssertGreaterThan(
            long, 55,
            "`not measured` measures under 55 pt at 10 pt, which does not match any system "
                + "font: the measurer is probably not using the font the card draws with")
    }
}
