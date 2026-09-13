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
        // The Fable tail's no-caption form (``Account/fableTailLabel``, when
        // the Fable window's reset equals the 7d row's own) — the common
        // case, since most fixtures learn both windows from the same poll.
        // The WITH-caption form ("fable 72% · in 3d 18h", the bridge's own
        // example) measures 118.8 pt at this size — already past even
        // ``testTheColumnDoesNotGrowWideEnoughToStarveTheBar``'s 96 pt
        // ceiling, so no width this column can safely take fits it. That
        // case elides through the same `.truncationMode(.middle)` the cost
        // tail already relies on; it is not asserted here because there is
        // no width to widen to that would make it pass.
        "fable 100%",
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
                + "text beside it; put the long string on its own line instead of widening "
                + "this column.")
    }

    /// The card draws TWO bar rows and no more. The model-scoped weekly
    /// window is a STRING in the second row's own trailing column
    /// (``AccountCard/rowTail(_:)``), which is where the pre-v4 card drew it
    /// too — a third bar row costs every card a measured 21 pt, and a caption
    /// line under the bars (the v4 transcription, then briefly reverted to
    /// only to be reverted again) costs every fable card a measured line of
    /// height. Gil, 2026-09-13: "no like we had both … align it like we had
    /// before."
    func testTheModelScopedWindowIsATailStringAndNotAThirdBarRowOrACaptionLine() throws {
        let source = try panelSource("PanelV4/AccountCard.swift")
        let squashed = source.components(separatedBy: .whitespacesAndNewlines).joined()
        XCTAssertFalse(
            squashed.contains("QuotaWindowSpec(label:\"fable\""),
            "the model-scoped window is a bar row again — that is the 21 pt per card "
                + "this layout exists to give back")
        XCTAssertFalse(
            squashed.contains("fableLine"),
            "the caption line under the bars is back — the card is one line taller per "
                + "fable account again, which is what Gil asked to undo")
        XCTAssertTrue(
            squashed.contains(
                "account.fableTailLabel(now:now,sevenDayResetAtMs:account.sevenDayResetAtMs)"),
            "row 2's tail no longer reads from Account.fableTailLabel, so the panel now has "
                + "a second spelling of that string and the two can drift")
    }

    /// A card inside a group box is ``AccountCard/Shape/compact``, which is NOT
    /// the density preference of the same name (`V4.compact`). The window rows
    /// were gated on `shape == .full`, so every grouped account drew its name,
    /// its pills and no quota at all — seven of eighteen accounts on this
    /// machine — while the block's own comment said both shapes drew them.
    ///
    /// Pinned at the source, because the difference is a `ForEach` being inside
    /// or outside an `if`, and a rendered card cannot tell "no rows" from "no
    /// windows to draw".
    func testEveryCardDrawsItsWindowsWhetherOrNotItIsInAGroup() throws {
        let source = try panelSource("PanelV4/AccountCard.swift")
        let squashed = source.components(separatedBy: .whitespacesAndNewlines).joined()
        XCTAssertFalse(
            squashed.contains("ifshape==.full{ForEach(Array(quotaWindows"),
            "the quota rows are gated on the card's shape again, so accounts inside a group "
                + "draw no windows at all")
        // Comments survive whitespace-squashing, so an adjacency string here
        // would break every time the block's own doc comment is reworded. What
        // matters is that `shape` gates nothing structural: its two remaining
        // uses fold the plan into a grouped card's name row and drop a
        // redundant pill, neither of which removes a measurement.
        let gates =
            source
            .split(separator: "\n")
            .filter { $0.contains("shape ==") && !$0.contains("//") }
            .map { $0.trimmingCharacters(in: .whitespaces) }
        XCTAssertEqual(
            gates.count, 3,
            "`shape` gates \(gates.count) branches now, not the 3 that are about labels:\n"
                + gates.joined(separator: "\n")
                + "\nA new one that skips a bar or a caption hides a measurement on every "
                + "grouped card. Check what it removes before updating this count.")
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
