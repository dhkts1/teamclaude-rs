import XCTest

@testable import TcrBarCore

/// The peers read must stop spending children the moment the panel is not on
/// screen.
///
/// # What is driven and what is read
///
/// The gate and the counter are values in `TcrBarCore` and are driven for real
/// below: a loop spawns children through the same ``TcrTool/run(executable:arguments:stdin:)``
/// the app uses, and the children write down that they ran. The wiring that
/// puts the gate into the panel's read loop and the shell's answer into the
/// gate is a fact about an executable target the test bundle does not link
/// (`Package.swift`), so those two are asserted against the source, the shape
/// the other panel wiring tests already use. The instrument for the whole
/// behaviour is `--measure-idle`.
@MainActor
final class PeerIdleSpawnTests: XCTestCase {
    override func tearDown() {
        PeerPollGate.panelIsOnScreen = nil
        super.tearDown()
    }

    // MARK: - The gate

    /// No panel, no claim. The render harness, the previews and every other
    /// test are in this state, and none of them may be quietly switched off.
    func testAnUnregisteredGateReads() {
        PeerPollGate.panelIsOnScreen = nil
        XCTAssertTrue(PeerPollGate.shouldRead())
    }

    func testTheGateFollowsWhoeverRegistered() {
        var shown = true
        PeerPollGate.panelIsOnScreen = { shown }
        XCTAssertTrue(PeerPollGate.shouldRead())
        shown = false
        XCTAssertFalse(
            PeerPollGate.shouldRead(),
            "the gate answered from a value it read once, so a panel that closed after the "
                + "loop started would never be noticed")
    }

    // MARK: - The counter, and a loop under the gate

    /// A start, then a stop, then nothing: the loop is still alive and running
    /// its own ticks, and spawns not one child while the panel is away.
    ///
    /// The third window is what makes the second one mean anything. A loop that
    /// had simply died would also spawn nothing, and that is not the behaviour
    /// being asked for: a popover that closes does not reliably tear its
    /// content down, so the same task has to read again when the panel comes
    /// back.
    func testALoopUnderTheGateSpawnsNothingWhileThePanelIsAway() async throws {
        let log = try SpawnLog(directory: scratch())
        var shown = true
        PeerPollGate.panelIsOnScreen = { shown }

        let interval = 0.05
        let loop = Task { @MainActor in
            while !Task.isCancelled {
                if PeerPollGate.shouldRead() {
                    _ = try? TcrTool.run(executable: log.stub, arguments: ["peer", "ls", "--json"])
                }
                try? await Task.sleep(nanoseconds: UInt64(interval * 1_000_000_000))
            }
        }
        defer { loop.cancel() }

        try await settle(interval)
        let whileShown = log.mark()
        XCTAssertGreaterThan(
            whileShown, 0,
            "the loop spawned nothing while the panel was on screen, so the windows below "
                + "would report a loop that never ran as a loop that stopped")

        shown = false
        try await settle(interval)
        let afterStop = log.mark()
        XCTAssertEqual(
            afterStop, whileShown,
            "the read kept spawning with the panel away: "
                + "\(afterStop - whileShown) children in \(settleTicks) ticks")

        shown = true
        try await settle(interval)
        XCTAssertGreaterThan(
            log.mark(), afterStop,
            "the loop did not read again when the panel came back, so a reopened panel would "
                + "draw whatever the last visit left")
    }

    /// Every child, with the verb it ran, and a bucket for one nobody named.
    func testTheCounterBucketsEveryChildByVerb() {
        let counts = SpawnLog.counts(ofArgv: [
            "peer ls --json", "peer status --json", "peer ls --json",
            "status --json", "--version",
        ])
        XCTAssertEqual(counts.map(\.verb), ["peer ls", "(no verb)", "peer status", "status"])
        XCTAssertEqual(counts.map(\.count), [2, 1, 1, 1])
        XCTAssertEqual(SpawnLog.verb(ofArgv: "peer ls --json"), "peer ls")
        XCTAssertEqual(
            SpawnLog.verb(ofArgv: "status --json"), "status",
            "a one word verb took the flag after it as half its name")
    }

    /// The child writes the line, and the stamp on it is the child's own.
    func testTheStubRecordsTheArgvItWasRunWith() throws {
        let log = try SpawnLog(directory: scratch())
        let before = Int(Date().timeIntervalSince1970)
        _ = try TcrTool.run(executable: log.stub, arguments: ["peer", "status", "--json"])
        _ = try TcrTool.run(executable: log.stub, arguments: ["control", "--show"])
        XCTAssertEqual(log.argvLines(), ["peer status --json", "control --show"])
        XCTAssertEqual(log.argvLines(after: 1), ["control --show"])
        let stamp = try XCTUnwrap(log.entries().first?.at)
        XCTAssertGreaterThanOrEqual(stamp, before)
    }

    /// The read verbs answer something every decoder here accepts, which is
    /// what lets a measured panel draw its tabs instead of an error card.
    func testTheStubAnswersTheReadsThePanelNeeds() throws {
        let log = try SpawnLog(directory: scratch())
        let status = try TcrTool.run(executable: log.stub, arguments: ["status", "--json"])
        XCTAssertEqual(try Fleet.decode(status.stdout).accounts.count, 1)
        let sessions = try TcrTool.run(executable: log.stub, arguments: ["sessions", "--json"])
        XCTAssertEqual(try Fleet.decodeSessions(sessions.stdout).channel, .live)
        let peers = try TcrTool.run(executable: log.stub, arguments: ["peer", "ls", "--json"])
        XCTAssertTrue(try JSONDecoder().decode(PeerListDocument.self, from: peers.stdout).supported)
    }

    // MARK: - The wiring, read from the source

    func testTheReadLoopAsksTheGateEveryTick() throws {
        let start = try slice(
            source("apps/macos/Sources/TcrBar/PanelV4/PeersTabV4.swift"),
            from: "    func start() {", to: "    func stop() {")
        XCTAssertTrue(
            start.contains("if PeerPollGate.shouldRead() {"),
            "the peers read no longer asks whether the panel is on screen, so a closed panel "
                + "spends two children every three seconds again")
        XCTAssertTrue(
            start.contains("await self?.refresh()"),
            "the tick reads through something other than refresh(), and the gate above it may "
                + "now be sitting in front of nothing")
    }

    func testTheShellAnswersTheGateFromTheLivePanel() throws {
        let shell = try source("apps/macos/Sources/TcrBar/MenuBarShell.swift")
        XCTAssertTrue(
            shell.contains("PeerPollGate.panelIsOnScreen = { [weak self] in self?.popover.isShown"),
            "nothing answers the gate any more, so it falls back to reading forever")
    }

    // MARK: - Plumbing

    /// Long enough for several ticks of the loop above, so a window that found
    /// no spawn found none over several chances rather than one.
    private let settleTicks = 6

    private func settle(_ interval: Double) async throws {
        try await Task.sleep(nanoseconds: UInt64(interval * Double(settleTicks) * 1_000_000_000))
    }

    /// A directory of this test's own. Two tests writing one path would each
    /// report the other's children.
    private func scratch() -> URL {
        FileManager.default.temporaryDirectory
            .appendingPathComponent("tcrbar-spawn-log-\(UUID().uuidString)")
    }

    private func repoRoot() -> URL {
        URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent()  // this file -> TcrBarTests
            .deletingLastPathComponent()  // TcrBarTests -> Tests
            .deletingLastPathComponent()  // Tests -> apps/macos
            .deletingLastPathComponent()  // apps/macos -> apps
            .deletingLastPathComponent()  // apps -> repo root
    }

    private func source(_ relativePath: String) throws -> String {
        try String(contentsOf: repoRoot().appendingPathComponent(relativePath), encoding: .utf8)
    }

    /// The text between two anchors, so an assertion cannot match an identical
    /// line elsewhere in a two thousand line view.
    private func slice(_ contents: String, from: String, to: String) throws -> String {
        let start = try XCTUnwrap(contents.range(of: from), "anchor not found: \(from)")
        let rest = contents[start.upperBound...]
        let end = try XCTUnwrap(rest.range(of: to), "anchor not found: \(to)")
        return String(rest[..<end.lowerBound])
    }
}
