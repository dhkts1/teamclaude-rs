import XCTest

@testable import TcrBarCore

/// The row under the exit picker used to print its own `waiting for <peer>`
/// line directly under the switch, about forty points below the identical
/// amber pill the account's header already draws. One card, one word said
/// twice.
///
/// Source-reading, the technique `AccountCardExitPillWiringTests` already
/// uses here and for the same reason: `Package.swift` gives the test target
/// `TcrBarCore` alone, `AccountExitRow` is a view in the executable, and
/// ViewInspector is not a dependency.
final class AccountExitRowWaitingLineTests: XCTestCase {

    /// The row must not read `AccountExit.waitingPill(peers:)` at all: that
    /// call is what drew the second amber line, and the header pill
    /// (`AccountCard.exitWaitingPillText`) already carries the same fact.
    func testTheRowNoLongerReadsTheWaitingPill() throws {
        let row = try source("apps/macos/Sources/TcrBar/PanelV4/AccountExitRow.swift")
        XCTAssertFalse(
            row.contains("waitingPill"),
            "the row still reads AccountExit.waitingPill(peers:), so the amber "
                + "\"waiting for <peer>\" line is drawn a second time under the "
                + "picker, on top of the header pill that already carries it")
    }

    /// The note stays: it is the only place the counted wait
    /// (`AccountExit.note(peers:)`'s "Waiting Xs.") is drawn on this row.
    func testTheNoteIsStillDrawn() throws {
        let row = try source("apps/macos/Sources/TcrBar/PanelV4/AccountExitRow.swift")
        XCTAssertTrue(
            row.contains("exit.note(peers: peerRows)"),
            "the note is gone too, so nothing on this row explains what the "
                + "must switch costs")
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
}
