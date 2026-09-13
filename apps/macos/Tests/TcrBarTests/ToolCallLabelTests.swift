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

    /// A row whose command head the wire could not carry names its session.
    ///
    /// Command heads never reach `session-wire.json` (the privacy call in
    /// `src/session_wire.rs`), so every restored row arrives with none — 72 of
    /// 95 `slowest` entries on the live proxy, 2026-09-13 — and each of those
    /// rows printed the single word `Bash`. Whose call it was is the least this
    /// row can say and still be worth reading.
    func testARowWithNoCommandHeadNamesItsSession() {
        XCTAssertEqual(
            ToolCallLabel.headline(commandHead: nil, tool: "Bash", owner: "teamclaude-rs-bc"),
            "Bash · teamclaude-rs-bc")
        // An empty string is the same absence as nil — the wire's `Option` and a
        // head truncated to nothing must not print two different rows.
        XCTAssertEqual(
            ToolCallLabel.headline(commandHead: "", tool: "Agent", owner: "example-c2"),
            "Agent · example-c2")
    }

    /// A real command head is the row, untouched — the owner is said on the sub
    /// line and must not be appended here.
    func testACommandHeadIsTheHeadlineOnItsOwn() {
        XCTAssertEqual(
            ToolCallLabel.headline(
                commandHead: "cargo test --release", tool: "Bash", owner: "teamclaude-rs-bc"),
            "cargo test --release")
    }

    /// No owner to name (no session file, and an id that resolved to nothing):
    /// the bare tool, never a dangling separator.
    func testAnUnknownOwnerLeavesTheToolAlone() {
        XCTAssertEqual(
            ToolCallLabel.headline(commandHead: nil, tool: "Bash", owner: ""),
            "Bash")
    }
}
