import XCTest

@testable import TcrBarCore

/// The internet row's own left edge, and the retry button that hangs off it.
///
/// Source-reading, the technique `AccountCardExitPillWiringTests` already
/// uses here and for the same reason: `Package.swift` gives the test target
/// `TcrBarCore` alone, `PeerInternetRow` is a view in the executable, and
/// ViewInspector is not a dependency.
final class PeerInternetRowLayoutTests: XCTestCase {

    /// The state line used to be a sibling of the toggle, starting at the
    /// row's own left margin while the sub-line above it sat indented under
    /// the switch label. It must now be inside the same label column.
    func testTheStateLineIsInsideTheToggleLabel() throws {
        let row = try source("apps/macos/Sources/TcrBar/PeerInternetRow.swift")
        let label = try slice(row, from: "Toggle(isOn:", to: "\n        }\n    }\n}")
        XCTAssertTrue(
            label.contains("if let line = state.line {"),
            "the state line is drawn outside the toggle's own label column, so "
                + "the row still has two left edges")
    }

    /// The retry button lives in the same label column as the state line, not
    /// as a third sibling row with its own margin.
    func testTheRetryButtonIsInsideTheToggleLabel() throws {
        let row = try source("apps/macos/Sources/TcrBar/PeerInternetRow.swift")
        let label = try slice(row, from: "Toggle(isOn:", to: "\n        }\n    }\n}")
        XCTAssertTrue(
            label.contains("Ask the router again"),
            "the retry button is not drawn inside the toggle's label column")
    }

    /// The button appears only on the two states that ended without a path,
    /// or while a retry is already in flight.
    func testTheButtonIsGatedOnCanRetryOrRetrying() throws {
        let row = try source("apps/macos/Sources/TcrBar/PeerInternetRow.swift")
        XCTAssertTrue(
            row.contains("state.canRetry || state == .retrying"),
            "the retry button's own gate changed; confirm it still covers exactly "
                + "routerSilent, unreadable, and the in-flight retry itself")
    }

    // MARK: - Source helpers (same technique as AccountCardExitPillWiringTests)

    private func repoRoot() -> URL {
        URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent()  // this file -> TcrBarTests
            .deletingLastPathComponent()  // TcrBarTests -> Tests
            .deletingLastPathComponent()  // Tests -> apps/macos
            .deletingLastPathComponent()  // apps/macos -> apps
            .deletingLastPathComponent()  // apps -> repo root
    }

    private func source(_ relativePath: String) throws -> String {
        try String(
            contentsOf: repoRoot().appendingPathComponent(relativePath), encoding: .utf8)
    }

    private func slice(_ contents: String, from: String, to: String) throws -> String {
        guard let start = contents.range(of: from) else {
            throw Anchor.missing(from)
        }
        let rest = contents[start.upperBound...]
        guard let end = rest.range(of: to) else {
            throw Anchor.missing(to)
        }
        return String(rest[..<end.lowerBound])
    }

    private enum Anchor: Error, CustomStringConvertible {
        case missing(String)
        var description: String {
            switch self {
            case .missing(let anchor): return "anchor not found: \(anchor)"
            }
        }
    }
}
