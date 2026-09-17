import XCTest

@testable import TcrBarCore

/// What the Tools tab's duration pill prints, and what VoiceOver hears.
///
/// The finding these pin (review #4): a
/// call the proxy KILLED at the Bash timeout and one that merely ran long drew
/// the same string in the same pill, red against grey the only difference — and
/// the pill's accessibility label was that same string, so a listener was told
/// nothing at all about the kill.
///
/// The timeout arrives as a parameter rather than being read from `V4` here on
/// purpose: `V4` is in the executable target, which this test bundle does not
/// link, and an assertion that restated `600` would be an assertion about this
/// file. The view passes `V4.toolTimeoutSeconds`; what is under test is the
/// RULE, driven at a timeout of the test's own choosing.
final class ToolCallLabelTests: XCTestCase {
    func testDurationTiersAtOneMinute() {
        XCTAssertEqual(ToolCallLabel.duration(45), "45s")
        XCTAssertEqual(ToolCallLabel.duration(252), "4m 12s")
        XCTAssertEqual(ToolCallLabel.duration(600), "10m 0s")
    }

    /// The tier that only an UNCAPPED call can reach. A three hour `Agent` is
    /// ordinary in RUNNING NOW, and the two-tier form printed it `180m 3s`,
    /// which truncated to `180m…` in the Sessions tab's "oldest" column.
    func testDurationTiersAgainAtOneHour() {
        XCTAssertEqual(ToolCallLabel.duration(3600), "1h 0m")
        XCTAssertEqual(ToolCallLabel.duration(10803), "3h 0m")
        XCTAssertEqual(ToolCallLabel.duration(11100), "3h 5m")
        // The boundary belongs to the minute tier on its low side: 3599s is
        // still under an hour and must not round up into "1h 0m".
        XCTAssertEqual(ToolCallLabel.duration(3599), "59m 59s")
    }

    func testACallAtTheTimeoutSaysSoInWords() {
        XCTAssertEqual(ToolCallLabel.pill(seconds: 600, timeout: 600), "timed out")
        // Past the deadline — a kill lands a hair after the figure it was
        // measured against, and the pill must not fall back to the duration.
        XCTAssertEqual(ToolCallLabel.pill(seconds: 601.4, timeout: 600), "timed out")
    }

    func testACallThatMerelyRanLongKeepsItsDuration() {
        XCTAssertEqual(ToolCallLabel.pill(seconds: 583, timeout: 600), "9m 43s")
        XCTAssertNil(ToolCallLabel.spoken(seconds: 583, timeout: 600))
    }

    /// The spoken value is the whole point: it carries BOTH facts, the elapsed
    /// time the pill gave up and the kill the colour used to carry alone.
    func testTheSpokenValueNamesTheKillAndTheElapsedTime() {
        XCTAssertEqual(
            ToolCallLabel.spoken(seconds: 600, timeout: 600),
            "ran 10m 0s, killed at the 600 second timeout")
    }

    /// Two calls 17 seconds apart used to be indistinguishable to a reader who
    /// cannot see the tint. They are not any more, in print or out loud.
    func testTheKilledCallAndTheSlowOneDifferWithoutColour() {
        let killed = ToolCallLabel.pill(seconds: 600, timeout: 600)
        let slow = ToolCallLabel.pill(seconds: 583, timeout: 600)
        XCTAssertNotEqual(killed, slow)
        XCTAssertNotEqual(
            ToolCallLabel.spoken(seconds: 600, timeout: 600),
            ToolCallLabel.spoken(seconds: 583, timeout: 600))
    }

    /// A running call prints its elapsed time, and inside the warning band
    /// how long is left as well — never colour alone.
    func testARunningCallNamesTheSecondsLeftInsideTheWarningBand() {
        XCTAssertEqual(
            ToolCallLabel.running(elapsed: 580, remaining: 20, warnWithin: 60),
            "9m 40s · 20s left")
        XCTAssertEqual(
            ToolCallLabel.running(elapsed: 252, remaining: 348, warnWithin: 60),
            "4m 12s")
    }

    /// A tool with no cap — an Agent, a Read — can never be "20s from"
    /// anything, so it prints its age and stops. The first render of the
    /// redesigned tab printed "9m 43s · 17s left" beside an Agent call that
    /// drew no ring, which is the panel claiming a deadline it does not have.
    func testAnUncappedCallNeverPrintsSecondsLeft() {
        XCTAssertEqual(
            ToolCallLabel.running(elapsed: 583, remaining: nil, warnWithin: 60), "9m 43s")
    }

    /// A call the wire gave no start time draws an empty label rather than a
    /// made-up zero.
    func testACallWithNoStartTimePrintsNothing() {
        XCTAssertEqual(ToolCallLabel.running(elapsed: nil, remaining: nil, warnWithin: 60), "")
    }

    /// Past the deadline the clause floors at zero: a kill lands a hair after
    /// the deadline it was measured against, and "-2s left" is not a reading.
    func testSecondsLeftFloorsAtZero() {
        XCTAssertEqual(
            ToolCallLabel.running(elapsed: 602, remaining: -2, warnWithin: 60),
            "10m 2s · 0s left")
    }
}
