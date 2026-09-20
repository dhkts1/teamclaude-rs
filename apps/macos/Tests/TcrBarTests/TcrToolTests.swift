import XCTest

@testable import TcrBarCore

/// What ``TcrTool/run(executable:arguments:stdin:)`` costs the thread that
/// calls it, against a REAL subprocess.
///
/// Real, because the thing under test is how the wait ends. A stub would
/// report the stub's own wait. `/bin/sh -c 'exit 3'` starts, exits with a code
/// nobody else uses, and touches nothing: not the proxy, not a file, not a
/// config.
///
/// The guard is the wait, not the child. A wait that runs the calling thread's
/// run loop returns on the next tick rather than when the child exits, which
/// on the main thread rounds every read up to about 62.5 milliseconds however
/// cheap the child was. Every panel read that happens on the main thread pays
/// that, so the cost is charged to the person waiting for the panel to draw.
final class TcrToolWaitTests: XCTestCase {

    /// A child that exits at once costs the main thread what the child costs.
    ///
    /// **The test asserts it is on the main thread first.** Off the main thread
    /// there is no run loop to spin and the old wait was already fast, so a
    /// timing assertion that ran anywhere else would pass on the broken code
    /// and guard nothing.
    ///
    /// Five runs and the MEDIAN, not one run: the run loop's step is paid on
    /// every single call, so it moves the median, while a machine that was busy
    /// for one moment moves only one sample. Thirty milliseconds sits well
    /// above what the spawn itself costs (single digits, measured) and well
    /// below the step being guarded against (about 62.5, and measured at 64.8).
    func testAChildThatExitsAtOnceDoesNotCostTheMainThreadARunLoopTick() throws {
        XCTAssertTrue(
            Thread.isMainThread,
            "this test is only meaningful on the main thread, where the run loop is spun")

        let shell = URL(fileURLWithPath: "/bin/sh")
        let arguments = ["-c", "exit 3"]

        // One run before the timed ones. The first spawn in a process pays for
        // whatever the runtime sets up once, and that cost belongs to neither
        // the child nor the wait.
        _ = try TcrTool.run(executable: shell, arguments: arguments)

        var samples: [Double] = []
        for _ in 0..<5 {
            let started = DispatchTime.now().uptimeNanoseconds
            let output = try TcrTool.run(executable: shell, arguments: arguments)
            let took = Double(DispatchTime.now().uptimeNanoseconds - started) / 1_000_000
            samples.append(took)

            // The shape of `Output` is a contract with every caller, so the
            // exit code and both streams are asserted on each run rather than
            // once: a faster wait that returned before the child was reaped
            // would show up here as a zero exit code.
            XCTAssertEqual(output.exitCode, 3)
            XCTAssertEqual(output.stdout, Data())
            XCTAssertEqual(output.stderr, "")
        }

        let median = samples.sorted()[samples.count / 2]
        XCTAssertLessThan(
            median, 30,
            "median of \(samples.map { String(format: "%.1f", $0) }) ms on the main thread")
    }

    /// Off the main thread the same child costs the same, which is the control:
    /// it says the number above is about the wait and not about this machine
    /// having a slow `fork`.
    func testTheSameChildCostsTheSameOffTheMainThread() async throws {
        let samples = await Task.detached(priority: .userInitiated) { () -> [Double] in
            let shell = URL(fileURLWithPath: "/bin/sh")
            let arguments = ["-c", "exit 3"]
            _ = try? TcrTool.run(executable: shell, arguments: arguments)
            var out: [Double] = []
            for _ in 0..<5 {
                let started = DispatchTime.now().uptimeNanoseconds
                _ = try? TcrTool.run(executable: shell, arguments: arguments)
                out.append(Double(DispatchTime.now().uptimeNanoseconds - started) / 1_000_000)
            }
            return out
        }.value

        let median = samples.sorted()[samples.count / 2]
        XCTAssertLessThan(
            median, 30, "median of \(samples.map { String(format: "%.1f", $0) }) ms off the main thread")
    }
}
