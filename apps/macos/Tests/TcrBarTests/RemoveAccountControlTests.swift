import Foundation
import XCTest

@testable import TcrBarCore

/// Pure argument-building and classification for `tcr remove`. Same posture as
/// `GroupCommandTests`/`ControlAccountCommandTests`: nothing here spawns a
/// process — `ImageRenderer` cannot draw `Menu` contents at all, so the gear's
/// "Delete Account…" item is never covered by a snapshot either.
///
/// Account names are obviously fake — this repository is public.
final class RemoveAccountCommandTests: XCTestCase {

    private let alice = "alice@example.com"

    // MARK: - argument shape (bridge: normal name, a name needing shell
    // quoting, and the `--org` form)

    /// A normal query, passed positionally and verbatim.
    func testArgumentsWithAnOrdinaryName() {
        XCTAssertEqual(RemoveAccountCommand.arguments(query: alice), ["remove", alice])
    }

    /// `Process` takes an argument vector, not a shell command line, so a
    /// name containing characters a shell would need quoted (a space, a
    /// single quote) is still passed through completely unmodified — this
    /// is the one thing that must NOT happen here, unlike
    /// `GroupCommand.shellQuote`'s copy-to-clipboard text.
    func testArgumentsPassANameNeedingShellQuotingUnmodified() {
        XCTAssertEqual(
            RemoveAccountCommand.arguments(query: "o'brien's mac"),
            ["remove", "o'brien's mac"]
        )
    }

    /// `--org` narrows an ambiguous match, mirroring the CLI's own flag.
    func testArgumentsWithOrgAppendsTheFlag() {
        XCTAssertEqual(
            RemoveAccountCommand.arguments(query: alice, org: "acme"),
            ["remove", alice, "--org", "acme"]
        )
    }

    // MARK: - classification

    func testCleanExitZeroNoStderr() {
        XCTAssertEqual(RemoveAccountCommand.classify(exitCode: 0, stderr: ""), .clean)
    }

    func testExitZeroWithStderrIsSpokeNotClean() {
        XCTAssertEqual(
            RemoveAccountCommand.classify(exitCode: 0, stderr: "warning: proxy too old for this route\n"),
            .spoke(notice: "warning: proxy too old for this route")
        )
    }

    func testNonZeroExitIsFailed() {
        let outcome = RemoveAccountCommand.classify(exitCode: 1, stderr: "error: no match for query\n")
        guard case .failed(let failure) = outcome else {
            return XCTFail("expected .failed, got \(outcome)")
        }
        XCTAssertEqual(failure.exitCode, 1)
        XCTAssertEqual(failure.message, "error: no match for query")
    }

    func testFailureSummaryIsNeverEmpty() {
        let failure = RemoveAccountCommand.Failure(exitCode: 1, message: "")
        XCTAssertEqual(failure.summary, "delete failed (exit 1): no output")
    }
}

/// The mutation controller: in-flight tracking, failure surfacing, and the
/// "stopped, stays listed until restart" note that never clears itself —
/// same shape as `GroupControllerTests`.
@MainActor
final class RemoveAccountControllerTests: XCTestCase {

    private let alice = "alice@example.com"

    func testNeverRemovedByDefault() {
        let controller = RemoveAccountController()
        XCTAssertFalse(controller.needsRestart(alice))
    }

    func testNoFailureByDefault() {
        let controller = RemoveAccountController()
        XCTAssertNil(controller.failure(for: alice))
        XCTAssertFalse(controller.isPending(alice))
    }
}
