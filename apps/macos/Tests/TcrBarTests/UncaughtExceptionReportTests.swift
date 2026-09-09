import XCTest

@testable import TcrBarCore

final class UncaughtExceptionReportTests: XCTestCase {

    /// The reason is the whole point of the handler: the crash report already
    /// carries the frames and never carries this.
    func testReasonIsCarriedOnTheFirstLine() {
        let lines = UncaughtExceptionReport.lines(
            name: "NSInternalInconsistencyException",
            reason: "already had more Update Constraints in Window passes",
            callStack: [])
        XCTAssertEqual(lines.count, 1)
        XCTAssertTrue(lines[0].contains("NSInternalInconsistencyException"))
        XCTAssertTrue(lines[0].contains("Update Constraints in Window passes"))
    }

    /// "carried no message" and "the handler dropped it" must not look the same
    /// in a log; they call for different fixes.
    func testAbsentReasonRendersExplicitly() {
        let lines = UncaughtExceptionReport.lines(
            name: "NSGenericException", reason: nil, callStack: [])
        XCTAssertTrue(lines[0].hasSuffix("(no reason)"))
    }

    /// Every line is findable by one predicate, including the truncation notice.
    func testEveryLineCarriesTheMarker() {
        let lines = UncaughtExceptionReport.lines(
            name: "X", reason: "y",
            callStack: (0..<40).map { "frame \($0)" }, maxFrames: 3)
        XCTAssertTrue(lines.allSatisfy { $0.hasPrefix(UncaughtExceptionReport.marker) })
    }

    /// Bounded, and it says how much it dropped rather than ending silently.
    func testStackIsTruncatedAndTheRemainderIsReported() {
        let lines = UncaughtExceptionReport.lines(
            name: "X", reason: "y",
            callStack: (0..<40).map { "frame \($0)" }, maxFrames: 3)
        // 1 header + 3 frames + 1 truncation notice.
        XCTAssertEqual(lines.count, 5)
        XCTAssertTrue(lines[1].contains("frame 0"))
        XCTAssertTrue(lines[3].contains("frame 2"))
        XCTAssertTrue(lines[4].contains("37 more frames"))
    }

    /// A stack shorter than the bound gets no truncation notice at all.
    func testShortStackHasNoTruncationNotice() {
        let lines = UncaughtExceptionReport.lines(
            name: "X", reason: "y", callStack: ["a", "b"], maxFrames: 24)
        XCTAssertEqual(lines.count, 3)
        XCTAssertFalse(lines.contains { $0.contains("more frames") })
    }

    /// The handler runs in an undefined runtime state; a nonsense bound must
    /// not become a negative `prefix` or a negative remainder.
    func testNegativeBoundIsTreatedAsZero() {
        let lines = UncaughtExceptionReport.lines(
            name: "X", reason: "y", callStack: ["a", "b"], maxFrames: -5)
        XCTAssertEqual(lines.count, 2)
        XCTAssertTrue(lines[1].contains("2 more frames"))
    }
}
