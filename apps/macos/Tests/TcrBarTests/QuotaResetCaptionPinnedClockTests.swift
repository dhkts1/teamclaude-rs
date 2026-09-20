import XCTest

@testable import TcrBarCore

/// Regression coverage for the exits-card render fixture's reset captions.
///
/// `RenderStates.account()` and `RenderStates.exitsCard(_:)` live in the
/// `TcrBar` executable target and are not importable here
/// (`RenderFixtureShapeTests`'s own doc-comment on why: the test target
/// links `TcrBarCore` alone). These tests instead pin the `TcrBarCore`
/// contract that fixture depends on, ``QuotaFormat/resetCaption(resetAtMs:now:)``,
/// the same way `RenderFixtureShapeTests` pins the logic behind the
/// duplicate-scene bug it guards.
///
/// The bug: `account()`'s reset offsets used to be measured from `Date()`,
/// the real clock, while `exitsCard` renders its account against `peerNow`,
/// a clock fixed in the past. Two fixture builds on two different days
/// produced two different captions against the one render clock that never
/// moved: `w12-exits-local-dark` read "in 44d 18h" one week, "in 45d 2h"
/// the next, both against a 5h window. The fix, `RenderStates.exitsAliceJSON`,
/// measures the offset from `peerNow` too, so the two clocks are the same
/// clock and the gap between them can no longer grow.
final class QuotaResetCaptionPinnedClockTests: XCTestCase {

    private let peerNow = Date(timeIntervalSince1970: 1_786_000_000)

    /// The failure mode itself, reproducible right now: a reset offset
    /// measured from the real clock, read back against `peerNow`. The real
    /// clock has moved well past `peerNow` since this fixture's reference
    /// epoch was chosen, so the "5h" window reads as tens of days away
    /// instead, exactly the wrong-magnitude caption the PNGs showed.
    func testResetOffsetMeasuredFromTheRealClockMisreadsAgainstAFixedRenderClock() {
        let resetAtMs = Int64(Date().addingTimeInterval(130 * 60).timeIntervalSince1970 * 1000)

        let caption = QuotaFormat.resetCaption(resetAtMs: resetAtMs, now: peerNow)

        XCTAssertNotEqual(
            caption, "in 2h 10m",
            "a 130-minute window measured from the real clock and read back against a `now` "
                + "fixed months in the past must NOT still read as 2h 10m away. If it does, "
                + "the two clocks have drifted back into agreement by coincidence, not by fix")
    }

    /// The fix: the offset and the render clock are the same `peerNow`, so
    /// rebuilding the fixture on a different real-world day changes nothing.
    func testResetOffsetMeasuredFromTheSameClockAsTheRenderStaysStable() {
        let firstBuild = Int64(peerNow.addingTimeInterval(130 * 60).timeIntervalSince1970 * 1000)
        // A second build, simulated a month later in the real world: `peerNow`
        // is a constant, not `Date()`, so nothing about this computation
        // depends on when it runs.
        let secondBuild = Int64(peerNow.addingTimeInterval(130 * 60).timeIntervalSince1970 * 1000)

        let firstCaption = QuotaFormat.resetCaption(resetAtMs: firstBuild, now: peerNow)
        let secondCaption = QuotaFormat.resetCaption(resetAtMs: secondBuild, now: peerNow)

        XCTAssertEqual(firstCaption, secondCaption)
        XCTAssertEqual(
            firstCaption, "in 2h 10m",
            "130 minutes, the exits fixture's own 5h window, read back exactly")
    }
}
