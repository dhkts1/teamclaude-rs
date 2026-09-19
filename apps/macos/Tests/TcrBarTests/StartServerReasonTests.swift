import Foundation
import XCTest

@testable import TcrBarCore

/// `ServerController.State.startDisabledReason`, the plain sentence for a
/// disabled "Start server" control. A colleague's "Start server" was
/// disabled with no reason at all; this pins what the reason says once a
/// non-proxy holder is the cause, and that every other state stays silent.
final class StartServerReasonTests: XCTestCase {

    private let em = "\u{2014}"

    /// The real shape `takeover_port` (`src/singleton.rs`) prints ahead of the
    /// bind failure: the non-proxy holder line, then the failed-bind line
    /// `ServerController.classifyExit` matches on to reach `.incumbentHoldsPort`
    /// in the first place.
    private func nonProxyMessage(port: Int = 3456, pid: Int = 4242, cmd: String) -> String {
        "[tcr] :\(port) is held by a non-proxy process (pid \(pid)): \(cmd) \(em) "
            + "not replacing it; the bind will fail if it stays.\n"
            + "failed to bind 127.0.0.1:\(port): Address already in use (os error 48)"
    }

    func testNamesTheHolderAndThePort() {
        let state = ServerController.State.incumbentHoldsPort(
            message: nonProxyMessage(cmd: "OtherGatewayServer"))
        XCTAssertEqual(
            state.startDisabledReason,
            "Port 3456 is held by OtherGatewayServer. Take over stops it.")
    }

    /// The common, benign case: another `tcr` already holds the port. No
    /// non-proxy line was ever printed, so there is nothing to name and
    /// starting again is not futile: it just reports "already running".
    func testNilWhenAnotherTcrHoldsThePort() {
        let state = ServerController.State.incumbentHoldsPort(
            message: "[tcr] another proxy holds :3456 (pid 55) and it is still listening")
        XCTAssertNil(state.startDisabledReason)
    }

    func testNilWhenSupervisingOurOwnChild() {
        XCTAssertNil(ServerController.State.supervising(pid: 1).startDisabledReason)
    }

    func testNilWhenTheHolderAnsweredNothing() {
        XCTAssertNil(
            ServerController.State.incumbentNotAnswering(message: "no answer").startDisabledReason)
    }

    func testNilWhenIdle() {
        XCTAssertNil(ServerController.State.idle.startDisabledReason)
    }
}
