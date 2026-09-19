import XCTest

@testable import TcrBarCore

/// The account card's exit-waiting pill: whether the exit state can be seen
/// at a glance, and whether it crowds the account's own name off the row.
///
/// Source-reading, the technique `PeersPanelViewWiringTests` already uses here
/// and for the same reason: `Package.swift` gives the test target
/// `TcrBarCore` alone, `AccountCard` is a view in the executable, and
/// ViewInspector is not a dependency.
///
/// A must-locked account whose exit Mac is down used to draw a lone green
/// `OK` pill, with the refusal buried in the note two lines below. The row
/// read as healthy at a glance.
///
/// Drawing the waiting pill inside the header row, beside OK, crowds
/// `alice @example.com` off the row entirely at the panel's real width
/// (`V4.panelWidth`, 372 pt, `PeerPaneLayoutTests`'s own figure), which is
/// why the pill is its own row directly under the header rather than
/// sharing it.
final class AccountCardExitPillWiringTests: XCTestCase {

    /// The card must reach `AccountExit`'s own waiting state, not stop at the
    /// quota classifier. A card that never asks `exit` anything cannot draw
    /// the one fact that keeps a must-locked, peer-down account from reading
    /// as healthy.
    func testTheCardReachesTheExitsWaitingState() throws {
        let card = try source("apps/macos/Sources/TcrBar/PanelV4/AccountCard.swift")
        XCTAssertTrue(
            card.contains("exit?.waitingPill(peers:") || card.contains(".waitingPill(peers:"),
            "nothing in the file reads AccountExit.waitingPill(peers:) any more, so the card "
                + "cannot draw the state that makes a must-locked, peer-down account NOT read "
                + "as healthy")
        XCTAssertTrue(
            card.contains("V4Pill(text: waiting, role: .warn"),
            "the waiting readout is no longer drawn as a pill, so the row loses the visual "
                + "weight OK carries")
    }

    /// The pill itself must NOT be crammed into the header's trailing
    /// `HStack` alongside the name. That is the exact layout this test
    /// exists to catch regressing back to: three badges plus the account's
    /// own name do not fit in 372 pt, and the name is what gives way. The
    /// header IS allowed to read `exitWaitingPillText` to decide whether to
    /// draw the state pill at all: that gate has no `role: .warn` pill of
    /// its own, so it does not add the badge this test guards against.
    func testTheWaitingPillIsNotInsideTheNamesTrailingRow() throws {
        let card = try source("apps/macos/Sources/TcrBar/PanelV4/AccountCard.swift")
        let header = try slice(
            card, from: "} trailing: {", to: ".accessibilityElement(children: .combine)")
        XCTAssertFalse(
            header.contains("role: .warn"),
            "the waiting pill is back inside the header's trailing HStack, which crowds "
                + "the account name (\"alice @example.com\") off the row at the real 372 pt "
                + "panel width, confirmed by rendering the exits-must-waiting card")
    }

    /// The pill sits directly under the header, before the quota rows: the
    /// first thing read after the name, which is what "at a glance" means.
    func testTheWaitingPillSitsDirectlyUnderTheHeader() throws {
        let card = try source("apps/macos/Sources/TcrBar/PanelV4/AccountCard.swift")
        let afterHeader = try slice(
            card, from: ".accessibilityElement(children: .combine)",
            to: "ForEach(Array(quotaWindows.enumerated())")
        XCTAssertTrue(
            afterHeader.contains("exitWaitingPillText") && afterHeader.contains("role: .warn"),
            "the waiting pill is not between the header and the quota rows, so it is either "
                + "gone or buried below the meters where OK already read as the whole story")
    }

    /// While the waiting pill is showing, the state pill must not also
    /// draw: a card refusing requests must not still read green `OK` at a
    /// glance. The state pill's draw is gated on the same
    /// `exitWaitingPillText` the waiting row already reads.
    func testStatePillIsHiddenWhileTheWaitingPillShows() throws {
        let card = try source("apps/macos/Sources/TcrBar/PanelV4/AccountCard.swift")
        let header = try slice(
            card, from: "} trailing: {", to: ".accessibilityElement(children: .combine)")
        XCTAssertTrue(
            header.contains("if exitWaitingPillText == nil {")
                && header.contains("V4Pill(text: statePillText, role: statePillRole"),
            "the state pill is not gated on exitWaitingPillText being nil, so a card that is "
                + "refusing requests can still show a lone green OK pill")
    }

    // MARK: - Source helpers (same technique as PeersPanelViewWiringTests)

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
