import XCTest

@testable import TcrBarCore

final class SessionFilesTests: XCTestCase {
    private func makeTempDirectory() throws -> URL {
        let dir = FileManager.default.temporaryDirectory
            .appendingPathComponent("tcrbar-session-files-tests-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        return dir
    }

    /// A missing directory reads as no files at all, never a thrown error —
    /// `panel-tabs-bridge.md`: "Tolerate a missing directory … skip, never
    /// fail the poll." Most machines running `tcr` have never run Claude
    /// Code's own CLI, so this is the routine case, not the edge one.
    func testMissingDirectoryReadsAsEmpty() {
        let missing = FileManager.default.temporaryDirectory
            .appendingPathComponent("tcrbar-does-not-exist-\(UUID().uuidString)")
        XCTAssertTrue(SessionFiles.read(directory: missing).isEmpty)
    }

    /// A file that does not parse is skipped, not fatal — the sibling files
    /// in the same directory still decode.
    func testUnparsableFileIsSkippedWithoutFailingTheRead() throws {
        let dir = try makeTempDirectory()
        defer { try? FileManager.default.removeItem(at: dir) }

        try "not json at all".write(
            to: dir.appendingPathComponent("broken.json"), atomically: true, encoding: .utf8)
        let goodJSON = """
            {"sessionId":"good-session","cwd":"/Users/alice/git/example",
             "name":"example-c1","status":"busy"}
            """
        try goodJSON.write(
            to: dir.appendingPathComponent("good.json"), atomically: true, encoding: .utf8)

        let files = SessionFiles.read(directory: dir)
        XCTAssertEqual(files.count, 1)
        XCTAssertEqual(files["good-session"]?.name, "example-c1")
    }

    /// A non-`.json` file in the same directory is ignored outright, not
    /// attempted and skipped — it never counts against the read.
    func testNonJsonFilesAreIgnored() throws {
        let dir = try makeTempDirectory()
        defer { try? FileManager.default.removeItem(at: dir) }
        try "hello".write(to: dir.appendingPathComponent("notes.txt"), atomically: true, encoding: .utf8)
        XCTAssertTrue(SessionFiles.read(directory: dir).isEmpty)
    }

    // MARK: JoinedSession

    private func session(id: String, account: String? = nil, lastSeenMs: Int64 = 0) -> Session {
        Session(
            sessionId: id, account: account, model: "claude-opus-5", firstSeenMs: 0, lastSeenMs: lastSeenMs)
    }

    /// `panel-tabs-bridge.md`: "A wire session with no file shows its id's
    /// first 8 chars and no project."
    func testWireSessionWithNoFileShowsIdHeadAndNoProject() {
        let joined = JoinedSession(session: session(id: "11111111-aaaa-bbbb"), file: nil)
        XCTAssertEqual(joined.displayName, "11111111")
        XCTAssertNil(joined.project)
        XCTAssertEqual(joined.activity, .unknown)
    }

    /// A joined session reads its name and project from the file, and its
    /// activity from the file's `status`.
    func testJoinedSessionReadsNameProjectAndActivityFromTheFile() {
        let file = SessionFile(
            sessionId: "11111111-aaaa-bbbb", cwd: "/Users/alice/git/teamclaude-rs",
            name: "teamclaude-rs-c7", status: "busy")
        let joined = JoinedSession(session: session(id: "11111111-aaaa-bbbb"), file: file)
        XCTAssertEqual(joined.displayName, "teamclaude-rs-c7")
        XCTAssertEqual(joined.project, "teamclaude-rs")
        XCTAssertEqual(joined.activity, .busy)
    }

    /// A file with no matching wire session is dropped — `panel-tabs-bridge.md`:
    /// "it never went through the proxy" — so the join always returns exactly
    /// `sessions.count` rows, never `files.count`.
    func testAFileWithNoMatchingWireSessionIsDropped() {
        let sessions = [session(id: "s1")]
        let files = [
            "s1": SessionFile(sessionId: "s1", name: "kept"),
            "orphan": SessionFile(sessionId: "orphan", name: "never went through the proxy"),
        ]
        let joined = SessionJoin.join(sessions: sessions, files: files)
        XCTAssertEqual(joined.count, 1)
        XCTAssertEqual(joined[0].displayName, "kept")
    }

    func testAgeLabelTiers() {
        let joined = JoinedSession(session: session(id: "s1", lastSeenMs: 0), file: nil)
        let now = Date(timeIntervalSince1970: 5 * 60)  // 5 minutes later
        XCTAssertEqual(joined.ageLabel(now: now), "5m")
    }
}
