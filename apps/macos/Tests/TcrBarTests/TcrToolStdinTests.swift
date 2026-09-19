import XCTest

@testable import TcrBarCore

/// ``TcrTool/run(executable:arguments:stdin:)``'s stdin half, against REAL
/// subprocesses.
///
/// Real ones and not a stub, because the failure being guarded is a signal
/// disposition: a write to a pipe whose far end has closed raises `SIGPIPE`,
/// and its default disposition terminates THIS process, the menu-bar app, not
/// the child. No mock reproduces that, and a test that stubbed the pipe would
/// pass on the broken code.
///
/// `/bin/cat`, `/bin/echo` and `/usr/bin/false` are the three children: one
/// reads its whole stdin, one exits without reading a byte, and one exits
/// non-zero without reading. None of them is `tcr` and none of them touches the
/// live proxy.
final class TcrToolStdinTests: XCTestCase {

    /// The round trip: what goes in on stdin comes back on stdout. This is the
    /// path `tcr peer join --stdin` and the `tcr://` handler take.
    func testStdinReachesTheChildAndItsOutputComesBack() throws {
        let output = try TcrTool.run(
            executable: URL(fileURLWithPath: "/bin/cat"), arguments: [],
            stdin: "tcr://peer/join?v=1&nk=AAAABBBB")
        XCTAssertEqual(output.exitCode, 0)
        XCTAssertEqual(
            String(data: output.stdout, encoding: .utf8), "tcr://peer/join?v=1&nk=AAAABBBB")
    }

    /// The pipe is CLOSED after the write, or a child that reads to EOF waits
    /// forever on a parent still holding the write end, and this function
    /// would block in `readDataToEndOfFile` with nothing arriving. `/bin/cat`
    /// only exits when its stdin closes, so this test completing at all is the
    /// assertion.
    func testTheWriteEndIsClosedSoAChildReadingToEofFinishes() throws {
        let output = try TcrTool.run(
            executable: URL(fileURLWithPath: "/bin/cat"), arguments: [], stdin: "one line\n")
        XCTAssertEqual(output.exitCode, 0)
    }

    /// **The crash this fix exists for.** `/bin/echo` exits immediately and
    /// reads nothing, so by the time the parent writes, the read end is gone
    /// and the write returns `EPIPE`. With `SIGPIPE` at its default
    /// disposition the whole test process dies here, no failure message, no
    /// assertion, the runner reports a crash.
    ///
    /// It is the real shape of the bug, not a contrived one: `tcr peer join
    /// --stdin` against a spent invite, and clap against an unknown flag, both
    /// exit before reading. Every `--scope` and `--for` control added here
    /// hits that path against the `tcr` in this tree.
    ///
    /// **The payload is deliberately larger than the pipe buffer.** A short
    /// write is a RACE: 4 KiB lands in the 64 KiB buffer whether or not the
    /// child has got round to exiting, so the write succeeds and the test
    /// passes on the broken code, measured, by watching this gate stay green
    /// against a restored `SIG_DFL`
    /// (`apps/macos/scripts/watch-peer-panel-view-gates-fail.sh`, first run). Past
    /// the buffer the write has to block, the child exits, the read end
    /// closes, and `SIGPIPE` arrives every time.
    func testWritingToAChildThatAlreadyExitedDoesNotKillThisProcess() throws {
        let output = try TcrTool.run(
            executable: URL(fileURLWithPath: "/bin/echo"), arguments: ["done"],
            stdin: String(repeating: "x", count: 256 * 1024))
        XCTAssertEqual(output.exitCode, 0)
        XCTAssertEqual(String(data: output.stdout, encoding: .utf8), "done\n")
    }

    /// And the child's own exit code survives: a refusal is reported by the
    /// process that made it, rather than being replaced by "broken pipe",
    /// which names the plumbing and not the problem.
    func testARefusingChildsExitCodeIsWhatIsReported() throws {
        let output = try TcrTool.run(
            executable: URL(fileURLWithPath: "/usr/bin/false"), arguments: [],
            stdin: "a secret nobody read")
        XCTAssertEqual(output.exitCode, 1)
    }

    /// A payload larger than the 64 KiB pipe buffer against a child that
    /// never reads: the write cannot complete, and it still must not take the
    /// app down or hang forever.
    func testALargePayloadToANonReadingChildStillReturns() throws {
        let output = try TcrTool.run(
            executable: URL(fileURLWithPath: "/usr/bin/false"), arguments: [],
            stdin: String(repeating: "y", count: 256 * 1024))
        XCTAssertEqual(output.exitCode, 1)
    }

    /// With no stdin the child inherits this process's own, which is what
    /// every other verb has always done. Nothing about the fix changes that
    /// path.
    func testNoStdinLeavesTheChildsInputAlone() throws {
        let output = try TcrTool.run(
            executable: URL(fileURLWithPath: "/bin/echo"), arguments: ["plain"])
        XCTAssertEqual(output.exitCode, 0)
        XCTAssertEqual(String(data: output.stdout, encoding: .utf8), "plain\n")
    }
}
