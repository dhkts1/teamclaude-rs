import XCTest

@testable import TcrBarCore

/// What Compact's one-line quota block speaks — the reset every window's
/// caption used to carry, moved to this line's own accessibility value and
/// `.help` tooltip once three ``QuotaRow`` lines folded onto one
/// (`data/plans/dense-quota-bridge.md`).
///
/// `swift test` links `TcrBarCore` only, so `DenseQuotaLine` itself cannot be
/// built here; what it speaks is `QuotaFormat.denseLineSpokenValue`, which can
/// — the same split `QuotaRowSpokenValueTests` already draws for `QuotaRow`.
final class DenseQuotaLineSpokenValueTests: XCTestCase {

    private let now = Date(timeIntervalSince1970: 1_757_000_000)

    private func resetIn(minutes: Int) -> Int64 {
        Int64((now.timeIntervalSince1970 + Double(minutes) * 60) * 1000)
    }

    func testAllThreeWindowsAreSpokenWithTheirOwnResets() {
        let spoken = QuotaFormat.denseLineSpokenValue(
            windows: [
                (label: "5h", value: 0.19, state: .ok, resetAtMs: resetIn(minutes: 86)),
                (label: "7d", value: 0.03, state: .ok, resetAtMs: resetIn(minutes: 9_360)),
                (label: "fable", value: 0.0, state: .ok, resetAtMs: nil),
            ], now: now)
        XCTAssertEqual(
            spoken,
            "5h window, 19% used, within limit, resets 1h 26m; "
                + "7d window, 3% used, within limit, resets 6d 12h; "
                + "fable window, 0% used, within limit")
    }

    /// A card the server never measured says so, rather than inventing a `0%`
    /// for a window nothing has probed — the same rule ``QuotaRow`` follows.
    func testAnUnmeasuredWindowSaysNeverMeasuredNotZero() {
        let spoken = QuotaFormat.denseLineSpokenValue(
            windows: [
                (label: "5h", value: nil, state: nil, resetAtMs: nil)
            ], now: now)
        XCTAssertEqual(spoken, "5h window, never measured")
    }

    /// A card with no Fable window speaks only the windows it actually draws
    /// — the two-window and three-window cards must not read alike.
    func testAWindowTheServerDidNotReportIsAbsentFromTheLine() {
        let twoWindows = QuotaFormat.denseLineSpokenValue(
            windows: [
                (label: "5h", value: 0.5, state: .ok, resetAtMs: nil),
                (label: "7d", value: 0.5, state: .ok, resetAtMs: nil),
            ], now: now)
        XCTAssertFalse(twoWindows.contains("fable"))
    }
}
