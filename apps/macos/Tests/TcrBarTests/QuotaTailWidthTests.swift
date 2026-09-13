import AppKit
import XCTest

@testable import TcrBarCore

/// The quota rows' trailing column is a FIXED width, so a string wider than it
/// does not wrap or push — it is ELIDED, and `truncationMode(.middle)` takes
/// the middle out. For a money figure the middle is the number: `$1,190 · 3.1M`
/// drew as `$1,1…3.1M`, which is not a shorter figure, it is a wrong one.
///
/// Nothing caught that. The suite was green, `V4.usageTailWidth` was a number
/// nobody had measured a string against, and the defect was visible only in a
/// render. This file is that measurement, in the test suite, using the same
/// font the panel draws with.
///
/// `V4` lives in the app target and this bundle only links `TcrBarCore`, so the
/// tokens are read out of the SOURCE — the same thing `PanelV4ControlsTests`
/// does, for the same reason.
///
/// Fixtures use obviously-fake account names only — see CLAUDE.md.
final class QuotaTailWidthTests: XCTestCase {

    /// Every string ``AccountCard`` can put in the trailing column: the spend
    /// tail on the first row, the plan name on the second. Chosen at the wide
    /// end of plausible rather than the typical one, because the column is
    /// sized once for all of them and a four-figure spend is an ordinary
    /// weekend here.
    private let tailStrings = [
        "$540 · 1.5M",  // the one the old 68 pt was sized for
        "$1,190 · 3.1M",  // the one that was eliding
        "$1,810 · 12.3M",
        "$12,345 · 120M",
        "$1,190+ · 3.1M",  // the `+` an unpriced request adds
        "Max 20x",
        "Team Standard",
    ]

    func testEveryTrailingStringFitsTheColumnItIsDrawnIn() throws {
        let width = try token("usageTailWidth")
        let size = try muteSize()
        for string in tailStrings {
            let drawn = (string as NSString)
                .size(withAttributes: [.font: NSFont.systemFont(ofSize: size)]).width
            XCTAssertLessThanOrEqual(
                drawn, width,
                "\"\(string)\" needs \(String(format: "%.1f", drawn)) pt and the column is "
                    + "\(width) pt, so it draws elided. Widen V4.usageTailWidth to fit it, or "
                    + "move the string to its own line — do not let it truncate.")
        }
    }

    /// The column is not free: it is subtracted from the bar on BOTH rows of
    /// EVERY card, and the bar is the thing a reader actually compares. Sizing
    /// it for a whole window caption once took the bar from 154 pt to 48 pt.
    /// So the column has a ceiling as well as a floor, and a caption that wants
    /// more than this belongs on its own line.
    func testTheColumnDoesNotGrowWideEnoughToStarveTheBar() throws {
        let width = try token("usageTailWidth")
        XCTAssertLessThanOrEqual(
            width, 96,
            "V4.usageTailWidth is \(width) pt. Past ~96 the quota bar is narrower than the "
                + "text beside it; put the long string on its own line (AccountCard.fableLine) "
                + "instead of widening this column.")
    }

    /// The card draws TWO bar rows. The model-scoped weekly window is a caption
    /// line under them (``Account/fableWeeklyLabel(now:)``), which is where it
    /// sat before v4 — a third bar row cost every card a measured 21 pt, and
    /// putting it in the trailing column cost both bars far more than that.
    func testTheModelScopedWindowIsACaptionLineAndNotAThirdBarRow() throws {
        let source = try panelSource("PanelV4/AccountCard.swift")
        let squashed = source.components(separatedBy: .whitespacesAndNewlines).joined()
        XCTAssertFalse(
            squashed.contains("QuotaWindowSpec(label:\"fable\""),
            "the model-scoped window is a bar row again — that is the 21 pt per card "
                + "this layout exists to give back")
        XCTAssertTrue(
            squashed.contains("account.fableWeeklyLabel(now:now)"),
            "the caption line no longer uses Account.fableWeeklyLabel, so the panel now has "
                + "a second spelling of that string and the two can drift")
        // ADJACENCY, not presence: `fableLine` also NAMES the declaration below,
        // so `contains("fableLine")` stays true with the draw call deleted —
        // watched, 2026-09-13, that exact mutation exited 0 against it. What is
        // pinned here is the CALL: the line sits right after the window loop's
        // closing brace, inside the `shape == .full` block, so it draws under
        // the bars and only on the shape that has them.
        XCTAssertTrue(
            squashed.contains("trailingHelp:planLine)}fableLine}"),
            "the fable caption line is no longer drawn directly under the quota rows "
                + "(it may still be declared — that is not the same thing)")
    }

    // MARK: - Reading the tokens out of the app target's source

    private func token(_ name: String) throws -> CGFloat {
        let source = try panelSource("PanelV4/V4.swift")
        let pattern = "static let \(name): CGFloat = ([0-9.]+)"
        let value = try XCTUnwrap(
            firstCapture(of: pattern, in: source),
            "V4.\(name) is no longer a plain `static let … = <number>`; this test reads it "
                + "out of the source and must be taught the new shape")
        return try XCTUnwrap(CGFloat(Double(value) ?? .nan).isNaN ? nil : CGFloat(Double(value)!))
    }

    /// Comfortable's mute size — the LARGER of the two, so a column that fits
    /// at this size fits at Compact's too. One constant serves both densities.
    private func muteSize() throws -> CGFloat {
        let source = try panelSource("PanelV4/V4.swift")
        let squashed = source.components(separatedBy: .whitespacesAndNewlines).joined()
        let value = try XCTUnwrap(
            firstCapture(of: "muteSize:CGFloat\\{compact\\?[0-9.]+:([0-9.]+)\\}", in: squashed),
            "V4.muteSize is no longer `compact ? <n> : <n>`; teach this test the new shape "
                + "rather than guessing the font size the column is measured at")
        return CGFloat(try XCTUnwrap(Double(value)))
    }

    private func firstCapture(of pattern: String, in source: String) -> String? {
        guard let regex = try? NSRegularExpression(pattern: pattern),
            let match = regex.firstMatch(
                in: source, range: NSRange(source.startIndex..., in: source)),
            let range = Range(match.range(at: 1), in: source)
        else { return nil }
        return String(source[range])
    }

    private func panelSource(_ relative: String) throws -> String {
        let repoRoot = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent()  // -> TcrBarTests
            .deletingLastPathComponent()  // -> Tests
            .deletingLastPathComponent()  // -> apps/macos
            .deletingLastPathComponent()  // -> apps
            .deletingLastPathComponent()  // -> repo root
        let file = repoRoot.appendingPathComponent("apps/macos/Sources/TcrBar/\(relative)")
        return try String(contentsOf: file, encoding: .utf8)
    }
}
