import Foundation
import XCTest

@testable import TcrBarCore

/// `ClaudeRouteRead` is the Accounts tab banner's own answer to "where is
/// Claude routed", read straight from the user-level settings file rather
/// than from a fuller diagnostic that reads more sources. Every case here
/// points `home` at a throwaway temp directory, never the real one, and
/// never `~`.
final class ClaudeRouteReadTests: XCTestCase {

    private var tempHome: URL!

    override func setUpWithError() throws {
        tempHome = FileManager.default.temporaryDirectory
            .appendingPathComponent("claude-route-read-tests-\(UUID().uuidString)")
        try FileManager.default.createDirectory(
            at: tempHome.appendingPathComponent(".claude"), withIntermediateDirectories: true)
    }

    override func tearDownWithError() throws {
        try? FileManager.default.removeItem(at: tempHome)
    }

    func testDefaultsWhenNoSettingsFileExists() {
        let route = ClaudeRouteRead.current(home: tempHome)
        XCTAssertEqual(route.url, ClaudeRouteRead.defaultBaseURL)
        XCTAssertEqual(route.source, "default")
    }

    func testReadsTheEnvBlocksBaseURLWhenPresent() throws {
        let settings = """
            {"env":{"ANTHROPIC_BASE_URL":"http://127.0.0.1:9443"}}
            """
        try Data(settings.utf8).write(
            to: tempHome.appendingPathComponent(".claude/settings.json"))
        let route = ClaudeRouteRead.current(home: tempHome)
        XCTAssertEqual(route.url, "http://127.0.0.1:9443")
        XCTAssertEqual(route.source, "settings.json")
    }

    /// A settings file with no `env` block, or an empty URL, is the same as
    /// no file at all: the default, not a crash and not an empty string on
    /// screen.
    func testFallsBackToDefaultWhenTheEnvBlockHasNoBaseURL() throws {
        try Data("{}".utf8).write(
            to: tempHome.appendingPathComponent(".claude/settings.json"))
        let route = ClaudeRouteRead.current(home: tempHome)
        XCTAssertEqual(route.url, ClaudeRouteRead.defaultBaseURL)
        XCTAssertEqual(route.source, "default")
    }
}
