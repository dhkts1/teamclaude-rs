import XCTest

@testable import TcrBarCore

final class ControlAccountTests: XCTestCase {
    func testAssignArgumentsPassTheExactNamePositionally() {
        XCTAssertEqual(
            ControlCommand.arguments(name: "alice@example.com"),
            ["control", "alice@example.com"]
        )
    }

    func testClearArgumentsUseTheDedicatedFlagNotAName() {
        XCTAssertEqual(ControlCommand.clearArguments(), ["control", "--clear"])
    }

    func testTheNameIsNeverTruncatedOrFlagged() {
        // No `--org`, no leading dash that could be read as a flag.
        let name = "org-scoped+alias@example.com"
        let arguments = ControlCommand.arguments(name: name)
        XCTAssertEqual(arguments, ["control", name])
        XCTAssertFalse(arguments.contains("--org"))
    }

    // MARK: - classify, all three arms

    func testExitZeroWithASilentStderrIsTheOnlyCleanSuccess() {
        XCTAssertEqual(
            ControlCommand.classify(assigning: true, exitCode: 0, stderr: ""), .clean)
        // Whitespace-only stderr is still silence.
        XCTAssertEqual(
            ControlCommand.classify(assigning: true, exitCode: 0, stderr: " \n"), .clean)
    }

    func testExitZeroWithAnythingOnStderrIsNotClean() {
        // This is the rule the bridge names explicitly: exit 0 with stderr
        // output (e.g. "the running proxy is too old for this route") must
        // never be swallowed into a bare success.
        XCTAssertEqual(
            ControlCommand.classify(assigning: false, exitCode: 0, stderr: "some chatter"),
            .spoke(notice: "some chatter")
        )
        XCTAssertEqual(
            ControlCommand.classify(
                assigning: true, exitCode: 0,
                stderr: "warning: proxy too old for control route\n"
            ),
            .spoke(notice: "warning: proxy too old for control route")
        )
    }

    func testNonZeroExitIsReportedWithStderrVerbatim() {
        guard
            case .failed(let failure) = ControlCommand.classify(
                assigning: true, exitCode: 1, stderr: "no account matched\n")
        else {
            return XCTFail("expected .failed")
        }
        XCTAssertEqual(failure.assigning, true)
        XCTAssertEqual(failure.exitCode, 1)
        XCTAssertEqual(failure.message, "no account matched")
    }

    func testASilentFailureStillSaysSomething() {
        guard
            case .failed(let failure) = ControlCommand.classify(
                assigning: false, exitCode: 2, stderr: "")
        else {
            return XCTFail("expected .failed")
        }
        XCTAssertFalse(failure.summary.isEmpty)
        XCTAssertTrue(failure.summary.contains("no output"))
    }

    func testFailureSummaryNamesTheVerb() {
        let assign = ControlCommand.Failure(assigning: true, exitCode: 1, message: "boom")
        XCTAssertTrue(assign.summary.contains("set control account"))
        let clear = ControlCommand.Failure(assigning: false, exitCode: 1, message: "boom")
        XCTAssertTrue(clear.summary.contains("clear control account"))
    }
}
