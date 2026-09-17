import AppKit
import XCTest

@testable import TcrBarCore

/// Whether the segmented tab strip fits at FOUR tabs — `docs/design/panel-tabs.md`
/// §0: "the tab bar breaks at four, and that is measured". `feat/peers-panel`
/// declares a fourth `PanelTab.peers` case that has not landed on this branch,
/// so `PanelTab` itself is still three cases here — this test does not wait
/// for that merge to check the four-tab shape the bridge asked for; it reads
/// the three real labels straight out of `FleetView.swift`'s `PanelTab.title`
/// switch and adds the fourth ("Peers") as a literal, documented here as a
/// stand-in for the case this build does not declare yet.
///
/// `SegmentedTabs` is a SwiftUI view this bundle cannot instantiate (the test
/// target links `TcrBarCore` only — `Package.swift`), so the geometry is
/// measured the same way `QuotaTailWidthTests` measures a column it cannot
/// render: `V4` tokens read out of the app target's SOURCE, at the actual
/// system font, real string widths.
final class SegmentedTabsFitTests: XCTestCase {
    /// Every segment's badge count on the measured live fleet the bridge
    /// quotes: 18 accounts, 10 sessions, 3 tools running. Peers carries no
    /// badge — nothing on the wire counts peers yet.
    private let badges: [String: Int] = ["Accounts": 18, "Sessions": 10, "Tools": 3]

    /// Renders the bar at four tabs (three real, one "Peers" stand-in — see
    /// the class doc-comment) and fails while the Accounts segment is the
    /// widest of the four AND overflows its allotted width by a wide margin.
    /// Reads `PanelTab.title`'s REAL `case .accounts` string, so this is red
    /// before the label is shortened and green after — not two fixed-string
    /// fixtures that can never disagree with the code.
    ///
    /// It does NOT assert every segment fits its exact 84–85pt allotment:
    /// `SegmentedTabs` draws a 14pt icon per tab that the bridge's own CSS
    /// mockup does not (`docs/design/panel-tabs.md` §0 was measured against
    /// text + a count badge, no icon), so the real, icon-inclusive four-tab
    /// strip is tighter than the mockup implies — "Sessions", unchanged and
    /// out of scope here, still runs over on its own. What this gate owns is
    /// the ACCOUNTS segment: after the rename it must no longer be the worst
    /// offender, with only a small residual overflow left.
    func testAccountsSegmentIsNoLongerTheWidestOverflowOutlierAtFourTabs() throws {
        let accountsLabel = try panelTabTitle("accounts")
        let allotted = try allottedSegmentWidth(tabCount: 4)
        let widths = try idealWidths(accountsLabel: accountsLabel)
        let accountsWidth = try XCTUnwrap(widths[accountsLabel])
        let widest = try XCTUnwrap(widths.values.max())

        XCTAssertNotEqual(
            accountsWidth, widest,
            "\"\(accountsLabel)\" is still the widest of the four segments — shorten it "
                + "(PanelTab.title's `case .accounts`): \(widths)")
        XCTAssertLessThan(
            accountsWidth - allotted, 5,
            "\"\(accountsLabel)\" still overflows its segment by "
                + "\(String(format: "%.1f", accountsWidth - allotted)) pt — expected under 5pt "
                + "of residual overflow once shortened")
    }

    /// `[label: idealWidth]` for the four segments — the three real tabs plus
    /// the literal "Peers" stand-in (see the class doc-comment).
    private func idealWidths(accountsLabel: String) throws -> [String: CGFloat] {
        let sessions = try panelTabTitle("sessions")
        let tools = try panelTabTitle("tools")
        var out: [String: CGFloat] = [:]
        out[accountsLabel] = try idealSegmentWidth(
            label: accountsLabel, badge: badges["Accounts"])
        out[sessions] = try idealSegmentWidth(label: sessions, badge: badges[sessions])
        out[tools] = try idealSegmentWidth(label: tools, badge: badges[tools])
        out["Peers"] = try idealSegmentWidth(label: "Peers", badge: nil)
        return out
    }

    // MARK: - Geometry, read from V4's own source

    /// `(panelWidth - 2*panelPaddingSide - 2*segPadding - segGap*(n-1)) / n` —
    /// `SegmentedTabs`' own layout: an `HStack` of `n` equal-width buttons
    /// inside `.padding(segPadding)`, inside the panel's own side padding.
    private func allottedSegmentWidth(tabCount: Int) throws -> CGFloat {
        let panelWidth = try token("panelWidth")
        let panelPaddingSide = try token("panelPaddingSide")
        let segPadding = try token("segPadding")
        let segGap = try token("segGap")
        let available =
            panelWidth - 2 * panelPaddingSide - 2 * segPadding - segGap * CGFloat(tabCount - 1)
        return available / CGFloat(tabCount)
    }

    /// icon box + gap + label text + (badge gap + badge pill, if any) — the
    /// exact `HStack` `SegmentedTabs.item(_:)` lays out.
    private func idealSegmentWidth(label: String, badge: Int?) throws -> CGFloat {
        let iconBox = try token("tabIconBox")
        let tabGap = try token("tabGap")
        let labelSize = try token("tabLabelSize_comfortable")
        let tracking = try token("tabLabelTracking")
        let labelWidth = measure(label, size: labelSize, weight: .semibold) + tracking
        var width = iconBox + tabGap + labelWidth
        if let badge {
            let badgeSize = try token("badgeSize")
            let badgePaddingH = try token("badgePaddingH")
            let badgeWidth = measure("\(badge)", size: badgeSize, weight: .regular) + 2 * badgePaddingH
            width += tabGap + badgeWidth
        }
        return width
    }

    private func measure(_ string: String, size: CGFloat, weight: NSFont.Weight) -> CGFloat {
        (string as NSString)
            .size(withAttributes: [.font: NSFont.systemFont(ofSize: size, weight: weight)]).width
    }

    // MARK: - Reading tokens and labels out of the app target's source

    /// `PanelTab.title`'s own switch, one case at a time — pinned at the
    /// source for the same reason ``QuotaTailWidthTests`` reads `V4` there:
    /// this bundle does not link `TcrBar`.
    ///
    /// Scoped to the `enum PanelTab { … }` block specifically, not the whole
    /// file: `FleetView.swift` has at least one OTHER switch over the same
    /// cases with completely different strings (`v4FooterLeading`'s `case
    /// .sessions: return "proxy + Claude Code session files"`), and a
    /// file-wide `firstMatch` grabbed that one instead the first time this
    /// test was written.
    private func panelTabTitle(_ caseName: String) throws -> String {
        let source = try panelSource("FleetView.swift")
        let enumBody = try XCTUnwrap(
            firstCapture(of: "enum PanelTab[^{]*\\{([\\s\\S]*?)\\n\\}", in: source),
            "no `enum PanelTab { ... }` block found in FleetView.swift")
        let pattern = "case \\.\(caseName): return \"([^\"]+)\""
        return try XCTUnwrap(
            firstCapture(of: pattern, in: enumBody),
            "PanelTab.title no longer has a `case .\(caseName): return \"...\"` line in the "
                + "shape this test expects")
    }

    private func token(_ name: String) throws -> CGFloat {
        let source = try panelSource("PanelV4/V4.swift")
        // `tabLabelSize` is a density-computed `static var { compact ? 12 :
        // 12.5 }`, not a plain `static let`. The comfortable (12.5) branch is
        // read here, the larger of the two — a fit proven at the larger size
        // holds at the smaller one too, the same convention
        // `QuotaTailWidthTests.muteSize()` uses.
        if name == "tabLabelSize_comfortable" {
            let squashed = source.components(separatedBy: .whitespacesAndNewlines).joined()
            let value = try XCTUnwrap(
                firstCapture(of: "tabLabelSize:CGFloat\\{compact\\?[0-9.]+:([0-9.]+)\\}", in: squashed),
                "V4.tabLabelSize is no longer `compact ? <n> : <n>`")
            return CGFloat(try XCTUnwrap(Double(value)))
        }
        let pattern = "static let \(name): CGFloat = ([0-9.]+)"
        let value = try XCTUnwrap(
            firstCapture(of: pattern, in: source),
            "V4.\(name) is no longer a plain `static let … = <number>`")
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
