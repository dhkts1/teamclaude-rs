import Foundation
import XCTest

@testable import TcrBarCore

/// Pure argument-building and classification for `tcr group`. Same posture as
/// `ControlAccountCommandTests`: nothing here spawns a process.
///
/// Account and group names are obviously fake — this repository is public.
final class GroupCommandTests: XCTestCase {

    private let group = "codereview"
    private let account = "henry7@example.com"

    // MARK: - argument shape (bridge Unit 1)

    func testAddArguments() {
        XCTAssertEqual(
            GroupCommand.addArguments(group: group, account: account),
            ["group", "add", "codereview", "henry7@example.com"]
        )
    }

    func testRemoveArguments() {
        XCTAssertEqual(
            GroupCommand.removeArguments(group: group, account: account),
            ["group", "rm", "codereview", "henry7@example.com"]
        )
    }

    func testRemoveAllArguments() {
        XCTAssertEqual(
            GroupCommand.removeAllArguments(group: group),
            ["group", "rm", "codereview", "--all"]
        )
    }

    // MARK: - classification

    func testCleanExitZeroNoStderr() {
        XCTAssertEqual(GroupCommand.classify(exitCode: 0, stderr: ""), .clean)
    }

    func testExitZeroWithStderrIsSpokeNotClean() {
        XCTAssertEqual(
            GroupCommand.classify(exitCode: 0, stderr: "warning: proxy too old for this route\n"),
            .spoke(notice: "warning: proxy too old for this route")
        )
    }

    func testNonZeroExitIsFailed() {
        let outcome = GroupCommand.classify(exitCode: 1, stderr: "error: unknown group\n")
        guard case .failed(let failure) = outcome else {
            return XCTFail("expected .failed, got \(outcome)")
        }
        XCTAssertEqual(failure.exitCode, 1)
        XCTAssertEqual(failure.message, "error: unknown group")
    }

    func testFailureSummaryIsNeverEmpty() {
        let failure = GroupCommand.Failure(exitCode: 1, message: "")
        XCTAssertEqual(failure.summary, "group command failed (exit 1): no output")
    }
}

/// Client-side group-name validation, mirroring the CLI's own rule (bridge
/// Unit 3).
final class GroupNameValidationTests: XCTestCase {

    func testEmptyNameIsRejected() {
        XCTAssertEqual(GroupNameValidation.validate(""), .empty)
    }

    func testWhitespaceOnlyNameIsRejected() {
        XCTAssertEqual(GroupNameValidation.validate("   "), .empty)
    }

    func testControlCharacterIsRejected() {
        XCTAssertEqual(GroupNameValidation.validate("dev\u{0007}team"), .controlCharacter)
    }

    func testDeleteCharacterIsRejected() {
        XCTAssertEqual(GroupNameValidation.validate("dev\u{007F}team"), .controlCharacter)
    }

    func testCharacterAboveLatin1IsRejected() {
        // U+1F600 — well above the Latin-1 ceiling the CLI enforces.
        XCTAssertEqual(GroupNameValidation.validate("dev😀"), .aboveLatin1)
    }

    func testOrdinaryNameIsAccepted() {
        XCTAssertNil(GroupNameValidation.validate("codereview"))
    }

    func testLatin1ExtendedCharacterIsAccepted() {
        // U+00E9 (é) sits exactly at the Latin-1 boundary — must not be
        // rejected as "above" it.
        XCTAssertNil(GroupNameValidation.validate("café"))
    }
}

/// The mutation controller: in-flight tracking, failure surfacing, and the
/// "restart the proxy to apply" note.
@MainActor
final class GroupControllerTests: XCTestCase {

    private let group = "codereview"
    private let account = "henry7@example.com"

    func testNeverAppliedByDefault() {
        let controller = GroupController()
        XCTAssertFalse(controller.needsRestart(group))
    }

    func testNoFailureByDefault() {
        let controller = GroupController()
        XCTAssertNil(controller.failure(for: "\(group)/\(account)"))
        XCTAssertFalse(controller.isPending("\(group)/\(account)"))
    }
}
