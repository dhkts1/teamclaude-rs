import Foundation
import XCTest

@testable import TcrBarCore

/// Pure classification for `tcr control`. Same posture as
/// `ToggleVerdictTests`: `ImageRenderer` cannot draw `Menu` contents at all, so
/// the gear's "Use as control account" / "Clear control account" items are
/// never covered by a snapshot — this is the only place the logic behind them
/// is actually exercised.
///
/// Account names are obviously fake — this repository is public.
final class ControlAccountCommandTests: XCTestCase {

    private let alice = "alice@example.com"

    // MARK: - argument shape

    func testSetArgumentsPassTheNamePositionally() {
        XCTAssertEqual(ControlAccountCommand.setArguments(name: alice), ["control", alice])
    }

    func testSetArgumentsWithNilNameClear() {
        XCTAssertEqual(ControlAccountCommand.setArguments(name: nil), ["control", "--clear"])
    }

    // MARK: - `--show` classification

    /// The name printed on stdout, trimmed, is a `.set` reading.
    func testShowReportsTheNameWhenOneIsSet() {
        let reading = ControlAccountCommand.classifyShow(
            exitCode: 0, stdout: Data("\(alice)\n".utf8), stderr: "")
        XCTAssertEqual(reading, .set(alice))
    }

    /// `tcr`'s literal `(none)` sentinel is a real answer — "asked, and the
    /// answer is nothing" — not the same as not being able to ask at all.
    func testShowReportsNoneForTheSentinel() {
        let reading = ControlAccountCommand.classifyShow(
            exitCode: 0, stdout: Data("(none)\n".utf8), stderr: "")
        XCTAssertEqual(reading, .none)
    }

    /// A non-zero exit — the shape an older `tcr` with no `control` subcommand
    /// takes — must NOT collapse into `.none`. That is the exact defect this
    /// type exists to keep apart: "no control account is set" and "this build
    /// cannot answer the question" are different facts, and only one of them
    /// justifies a row drawing nothing.
    func testShowIsUnavailableOnANonZeroExit() {
        let reading = ControlAccountCommand.classifyShow(
            exitCode: 2, stdout: Data(), stderr: "error: unrecognized subcommand 'control'")
        guard case .unavailable(let reason) = reading else {
            return XCTFail("expected .unavailable, got \(reading)")
        }
        XCTAssertEqual(reason, "error: unrecognized subcommand 'control'")
    }

    @MainActor
    func testControllerNeverReportsControlWhenUnavailable() {
        let controller = ControlAccountController(pinned: alice, unavailable: true)
        // Pinned with a name AND marked unavailable — the pathological
        // combination a stale read could leave behind — must still read as "no
        // phantom checkmark", which is what `isControl` exists to guarantee.
        XCTAssertFalse(controller.isControl(alice))
    }

    @MainActor
    func testControllerReportsControlWhenAvailableAndMatching() {
        let controller = ControlAccountController(pinned: alice)
        XCTAssertTrue(controller.isControl(alice))
        XCTAssertFalse(controller.isControl("bob@example.com"))
    }

    // MARK: - set/clear classification

    func testSetIsCleanOnExitZeroWithNoStderr() {
        XCTAssertEqual(ControlAccountCommand.classifySet(exitCode: 0, stderr: ""), .clean)
    }

    /// Exit 0 with output on stderr is `tcr` reporting half-done work — same
    /// rule as `AccountCommand.classify`, mirrored here rather than assumed to
    /// still hold for a different subcommand.
    func testSetSpeaksUpOnExitZeroWithStderr() {
        let outcome = ControlAccountCommand.classifySet(
            exitCode: 0,
            stderr: "[tcr] WARNING: the proxy running on :3456 is too old to accept live "
                + "account control, so only the config file was changed.")
        guard case .spoke(let notice) = outcome else {
            return XCTFail("expected .spoke, got \(outcome)")
        }
        XCTAssertTrue(notice.contains("too old"))
    }

    func testSetFailsOnANonZeroExit() {
        let outcome = ControlAccountCommand.classifySet(
            exitCode: 1, stderr: "the proxy running on :3456 refused this: no match")
        guard case .failed(let failure) = outcome else {
            return XCTFail("expected .failed, got \(outcome)")
        }
        XCTAssertEqual(failure.exitCode, 1)
        XCTAssertTrue(failure.summary.contains("refused this"))
    }
}
