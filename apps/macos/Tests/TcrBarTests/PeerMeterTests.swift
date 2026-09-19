import XCTest

@testable import TcrBarCore

/// The Peers tab's row model, driven for real.
///
/// `PeersPanelWiringTests` may stay a source grep only
/// where a behaviour test is impossible. It was impossible for these because
/// `PeerMeter`, `LeaseFraction` and `GatewayBytes` sat in the `TcrBar`
/// executable target, which the test target does not link
/// (`Package.swift:39-43`); they are in `TcrBarCore` now, so the step the
/// running tab takes, a snapshot's meters to `[PeerPanelHeight.Row]` to the
/// height it frames its scroll view with, runs here with no view at all.
///
/// The metrics are the Comfortable figures `PeersTabV4.heightMetrics` passes
/// in, transcribed as literals for the reason `PeerSectionHeightTests` gives
/// in full: `V4` is in the executable target.
final class PeerMeterTests: XCTestCase {

    private let comfortable = PeerPanelHeight.Metrics(
        nameLineHeight: 21,
        subLineHeight: 17,
        meterHeight: 27,
        cardChrome: 22,
        cardGap: 14,
        fixedChrome: 216
    )

    private func lease(_ spent: Double) -> PeerMeter {
        .lease(LeaseFraction(spent: spent, sentence: "a third of the 7d window"))
    }

    private var gateway: PeerMeter {
        .gateway(
            GatewayBytes(
                bytesPerHour: 104_857_600, capBytesPerHour: 2_147_483_648,
                sentence: "against the 2 GB hourly ceiling"))
    }

    /// The height the tab frames its list to, taken the way the tab takes it:
    /// meters in, row shapes out, ``PeerPanelHeight`` last.
    private func viewport(_ meters: [PeerMeter]) -> CGFloat {
        PeerPanelHeight.listViewportHeight(
            rows: meters.map(\.rowShape), metrics: comfortable)
    }

    // MARK: - Meters to row shapes

    /// A row with no meter and no sentence is the name line and one sub line.
    /// A row with a sentence spends a second sub line on it, because that
    /// sentence is the one string on the tab with no bounded length.
    func testAnUnmeteredRowsShapeFollowsWhetherItHasASentence() {
        XCTAssertEqual(PeerMeter.none(nil).rowShape, .plain)
        XCTAssertEqual(
            PeerMeter.none("Trust compares six digits").rowShape,
            PeerPanelHeight.Row(subLines: 2, hasMeter: false))
    }

    /// Both meters are one bar and its label, so they cost the same height:
    /// the invariant ``PeerPanelHeight/Row/hasMeter`` states in as many words.
    /// A lease row that measured differently from a gateway row would make the
    /// drawn height depend on which meter the wire happened to report.
    func testBothMetersCostTheSameRowHeight() {
        XCTAssertEqual(lease(0.34).rowShape, .metered)
        XCTAssertEqual(gateway.rowShape, .metered)
        XCTAssertEqual(
            PeerPanelHeight.rowHeight(lease(0.34).rowShape, metrics: comfortable),
            PeerPanelHeight.rowHeight(gateway.rowShape, metrics: comfortable))
    }

    // MARK: - Meters to the height the view frames

    /// The tab's own budget figure, reached from the meters rather than from
    /// hand-built row shapes: seven trusted Macs with no meter want 518 pt,
    /// which clears the 520 pt cap by two points and is therefore drawn
    /// whole.
    func testSevenUnmeteredMacsAreDrawnWhole() {
        let meters = Array(repeating: PeerMeter.none(nil), count: 7)
        XCTAssertEqual(viewport(meters), 518)
        XCTAssertFalse(PeerPanelHeight.overflows(rows: meters.map(\.rowShape), metrics: comfortable))
    }

    /// And the case the cap exists for: seven Macs actually SERVING draw a
    /// meter and a scale sentence each, which is 133 pt over the cap, so the
    /// viewport clamps and the rows scroll inside it.
    func testSevenServingMacsClampTheViewportToTheCap() {
        let meters = Array(repeating: lease(0.34), count: 7)
        XCTAssertTrue(
            PeerPanelHeight.overflows(rows: meters.map(\.rowShape), metrics: comfortable),
            "seven metered rows fit under the cap, so this tab has no scrolling case left "
                + "and the clamp below is untested")
        XCTAssertEqual(viewport(meters), PanelHeight.panelMaxHeight)
    }

    /// Forty Macs are the same answer as eight: the clamp is what the view
    /// frames, so the drawn viewport never grows past the cap however long the
    /// list gets.
    func testTheViewportStopsGrowingAtTheCap() {
        XCTAssertEqual(
            viewport(Array(repeating: lease(0.5), count: 40)),
            viewport(Array(repeating: lease(0.5), count: 8)))
    }

    // MARK: - What the meters say

    /// Rule 5 of the mockup, as behaviour: a Mac that has stopped serving
    /// reads zero rather than its last value, and a spend over 1 draws a full
    /// bar rather than one wider than its track.
    func testALeaseMeterReadsZeroRatherThanItsLastValue() {
        XCTAssertEqual(LeaseFraction(spent: 0, sentence: "").value, "nothing yet")
        XCTAssertEqual(LeaseFraction(spent: 0.34, sentence: "").value, "34%")
        XCTAssertEqual(LeaseFraction(spent: 1.4, sentence: "").spent, 1)
        XCTAssertEqual(LeaseFraction(spent: -1, sentence: "").spent, 0)
    }

    /// The mockup's rule 2, as the two words a Mac lends and a Mac borrows: a
    /// direction is always named, never the bare word `shared`. One place
    /// decides the words, so the pill and the meter label cannot phrase the
    /// same fact two ways.
    func testPeerLendDirectionNamesTheWordsOnce() {
        XCTAssertEqual(PeerLendDirection.youLend.pillText, "you lend")
        XCTAssertEqual(PeerLendDirection.theyLend.pillText, "they lend")
        XCTAssertEqual(PeerLendDirection.youLend.meterLabel, "you lent")
        XCTAssertEqual(PeerLendDirection.theyLend.meterLabel, "they lent")
    }

    /// A lease meter's label says which Mac is the lender; it is never the
    /// direction-less `shared` the tab used to draw regardless of who was
    /// lending to whom.
    func testALeaseFractionCarriesItsDirectionsLabel() {
        let theyLend = LeaseFraction(
            spent: 0.62, sentence: "", label: PeerLendDirection.theyLend.meterLabel)
        XCTAssertEqual(theyLend.label, "they lent")
        let youLend = LeaseFraction(
            spent: 0.34, sentence: "", label: PeerLendDirection.youLend.meterLabel)
        XCTAssertEqual(youLend.label, "you lent")
        XCTAssertNotEqual(
            theyLend.label, youLend.label,
            "the two directions must not collapse back onto one shared word")
    }

    /// An unreported ceiling draws EMPTY, not full. A zero cap read as "at
    /// cap" would paint a full bar on a Mac nobody has measured.
    func testAnUnmeasuredCeilingDrawsEmpty() {
        let unmeasured = GatewayBytes(bytesPerHour: 1_000_000, capBytesPerHour: 0, sentence: "")
        XCTAssertEqual(unmeasured.fraction, 0)
        let measured = GatewayBytes(
            bytesPerHour: 1_073_741_824, capBytesPerHour: 2_147_483_648, sentence: "")
        XCTAssertEqual(measured.fraction, 0.5)
        XCTAssertEqual(measured.value, "1024 MB/hr")
        XCTAssertEqual(
            GatewayBytes(bytesPerHour: 0, capBytesPerHour: 1, sentence: "").value, "idle")
    }
}
