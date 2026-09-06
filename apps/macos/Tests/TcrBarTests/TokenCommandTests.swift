import Foundation
import XCTest

@testable import TcrBarCore

/// "Copy Access Token" is a subprocess (`tcr token <name>`) whose stdout goes
/// straight to the pasteboard. These pin the classification: the one case a
/// user cannot tell apart from success is an EMPTY copy, so exit 0 with no
/// stdout must be a failure.
///
/// Token values are obviously fake — this repository is public.
final class TokenCommandTests: XCTestCase {

    func testArgumentsPassQueryPositionally() {
        XCTAssertEqual(TokenCommand.arguments(query: "alice@example.com"), ["token", "alice@example.com"])
        XCTAssertEqual(
            TokenCommand.arguments(query: "alice@example.com", org: "acme"),
            ["token", "alice@example.com", "--org", "acme"])
    }

    func testExitZeroWithTokenIsSuccessTrimmed() {
        let out = Data("at-fake-alice\n".utf8)
        XCTAssertEqual(TokenCommand.classify(exitCode: 0, stdout: out, stderr: ""), .success("at-fake-alice"))
    }

    func testExitZeroWithEmptyStdoutIsFailure() {
        let result = TokenCommand.classify(exitCode: 0, stdout: Data(), stderr: "")
        guard case .failure(let failure) = result else {
            return XCTFail("an empty token must not reach the pasteboard")
        }
        XCTAssertEqual(failure.exitCode, 0)
        XCTAssertTrue(failure.summary.contains("no token"), failure.summary)
    }

    func testNonZeroExitCarriesStderrVerbatim() {
        let result = TokenCommand.classify(
            exitCode: 1, stdout: Data(), stderr: "Error: no account matches 'nobody'\n")
        XCTAssertEqual(
            result, .failure(TokenCommand.Failure(exitCode: 1, message: "Error: no account matches 'nobody'"))
        )
    }
}
