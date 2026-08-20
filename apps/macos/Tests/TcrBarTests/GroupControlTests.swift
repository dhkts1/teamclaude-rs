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

    // MARK: - copy-command text derives from the same argv the action runs

    /// The copied text and the argv the "Remove from <group>" button actually
    /// runs must agree — asserted by deriving one from the other, not by
    /// comparing two hand-typed strings, so a change to `removeArguments`
    /// can never leave the copy button lying about what it does.
    func testCopyCommandTextAgreesWithTheRemoveArgumentsItIsBuiltFrom() {
        let arguments = GroupCommand.removeArguments(group: group, account: account)
        XCTAssertEqual(
            GroupCommand.commandLine(arguments: arguments),
            "tcr " + arguments.joined(separator: " ")
        )
    }

    func testCopyCommandTextMatchesTheDocumentedExample() {
        let arguments = GroupCommand.removeArguments(group: "dev", account: "henry2@example.com")
        XCTAssertEqual(
            GroupCommand.commandLine(arguments: arguments),
            "tcr group rm dev henry2@example.com"
        )
    }

    func testShellQuoteLeavesAnEmailBare() {
        XCTAssertEqual(GroupCommand.shellQuote("henry2@example.com"), "henry2@example.com")
    }

    /// An account or group name is not guaranteed to be shell-safe — this is
    /// the one place that must not assume it is.
    func testShellQuoteQuotesANameContainingASpace() {
        XCTAssertEqual(GroupCommand.shellQuote("dev team"), "'dev team'")
    }

    func testShellQuoteEscapesAnEmbeddedSingleQuote() {
        XCTAssertEqual(GroupCommand.shellQuote("o'brien"), "'o'\\''brien'")
    }

    func testCommandLineQuotesAGroupNameThatNeedsIt() {
        let arguments = GroupCommand.removeArguments(group: "dev team", account: account)
        XCTAssertEqual(
            GroupCommand.commandLine(arguments: arguments),
            "tcr group rm 'dev team' henry7@example.com"
        )
    }

    // MARK: - CopyCommandMenuEntry (bridge Unit 1: unambiguous copy)

    /// The whole point of ``GroupCommand/CopyCommandMenuEntry``: the title a
    /// user reads and the text that lands on the clipboard both come out of
    /// the same `commandLine(arguments:)` call, so they can never drift.
    /// Asserted by deriving the expectation from that function too, not by
    /// comparing two hand-typed literals.
    func testCopyEntryTitleAndCopiedTextAreBothDerivedFromCommandLine() {
        let arguments = GroupCommand.removeArguments(group: group, account: account)
        let entry = GroupCommand.CopyCommandMenuEntry(arguments: arguments)
        let expected = GroupCommand.commandLine(arguments: arguments)
        XCTAssertEqual(entry.copiedText, expected)
        XCTAssertEqual(entry.title, "Copy \u{201C}\(expected)\u{201D}")
    }

    /// Same relationship, but for the add form offered on a group the
    /// account is not yet in — remove-only was half the affordance the
    /// bridge asked to close.
    func testCopyEntryForTheAddFormAlsoDerivesFromCommandLine() {
        let arguments = GroupCommand.addArguments(group: "dev", account: "henry2@example.com")
        let entry = GroupCommand.CopyCommandMenuEntry(arguments: arguments)
        XCTAssertEqual(entry.copiedText, "tcr group add dev henry2@example.com")
        XCTAssertEqual(entry.title, "Copy \u{201C}tcr group add dev henry2@example.com\u{201D}")
    }

    /// A remove-form entry and an add-form entry for the exact same
    /// group/account never collide — this is the failure mode that made
    /// "Copy tcr group Command" ambiguous in the first place (bridge: two
    /// identically-labelled items with no way to tell which one is which).
    func testRemoveAndAddCopyEntriesForTheSameGroupAndAccountDiffer() {
        let removeEntry = GroupCommand.CopyCommandMenuEntry(
            arguments: GroupCommand.removeArguments(group: group, account: account))
        let addEntry = GroupCommand.CopyCommandMenuEntry(
            arguments: GroupCommand.addArguments(group: group, account: account))
        XCTAssertNotEqual(removeEntry.title, addEntry.title)
        XCTAssertNotEqual(removeEntry.copiedText, addEntry.copiedText)
    }
}

/// Whether a typed name can be used to create a brand new group (bridge
/// Unit 1: create-a-group).
final class NewGroupNameTests: XCTestCase {

    func testEmptyNameIsRejected() {
        XCTAssertEqual(
            NewGroupName.evaluate("", existingGroups: []),
            .rejected(.empty)
        )
    }

    func testControlCharacterIsRejected() {
        XCTAssertEqual(
            NewGroupName.evaluate("dev\u{0007}team", existingGroups: []),
            .rejected(.controlCharacter)
        )
    }

    func testNameAboveLatin1IsRejected() {
        XCTAssertEqual(
            NewGroupName.evaluate("dev😀", existingGroups: []),
            .rejected(.aboveLatin1)
        )
    }

    /// The gap this feature exists to close: without this check, typing an
    /// existing group's name would silently perform a plain add into it
    /// instead of the "create a new group" the operator asked for.
    func testDuplicateOfAnExistingGroupIsRejected() {
        XCTAssertEqual(
            NewGroupName.evaluate("dev", existingGroups: ["dev", "codereview"]),
            .duplicate
        )
    }

    func testNovelValidNameIsAccepted() {
        XCTAssertEqual(
            NewGroupName.evaluate("staging", existingGroups: ["dev"]),
            .valid("staging")
        )
    }

    func testAcceptedNameIsTrimmed() {
        XCTAssertEqual(
            NewGroupName.evaluate("  staging  ", existingGroups: []),
            .valid("staging")
        )
    }

    func testRejectionMessageIsNilForAValidName() {
        XCTAssertNil(NewGroupName.evaluate("staging", existingGroups: []).rejectionMessage)
    }

    func testRejectionMessageNamesTheDuplicateReason() {
        let outcome = NewGroupName.evaluate("dev", existingGroups: ["dev"])
        XCTAssertEqual(outcome.rejectionMessage, "A group named that already exists.")
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
