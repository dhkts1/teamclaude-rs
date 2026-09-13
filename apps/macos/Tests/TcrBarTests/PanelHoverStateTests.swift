import XCTest

/// Every control on the v4 panel answers the pointer.
///
/// Before this, `rg onHover` across the whole target returned ZERO hits: a
/// button, a disclosure and a tab were indistinguishable from the card behind
/// them until clicked. `V4PressStyle` is the one place all four v4 controls
/// share (`V4Button`, `V4Disclosure`, `SegmentedTabs`, `PanelHeader`'s gear),
/// so the hover is written there once.
///
/// Source-reading, the technique `FleetViewContextMenuTests` already uses here
/// for an assertion a runtime test cannot reach cheaply: `ImageRenderer` has no
/// pointer, so `--render-states` can never photograph a hover, and
/// ViewInspector is not a dependency of this target.
final class PanelHoverStateTests: XCTestCase {

    func testThePressStyleCarriesAHoverFill() throws {
        let source = try panelV4Source("V4Button.swift")
        XCTAssertTrue(
            source.contains(".onHover { hovering = $0 }"),
            "V4PressStyle has no onHover — nothing on the panel answers the pointer")
        XCTAssertTrue(
            source.contains("Tok.ink.opacity(hovering ? V4.hoverFillAlpha : 0)"),
            "the hover fill is no longer additive over the control's own fill; "
                + "one value stops reproducing both the sheet's .more (0 to .07) "
                + "and .btn (.10 to .17)")
    }

    /// Reduce Motion drops the TRANSITION and keeps the hover, which is what
    /// the sheet does: its `.15s` lives inside `@media (prefers-reduced-motion:
    /// no-preference)`. The state change is information, not motion, and
    /// removing it would take pointer feedback away from the people most likely
    /// to need it.
    func testReduceMotionDropsTheTransitionAndKeepsTheHover() throws {
        let source = try panelV4Source("V4Button.swift")
        XCTAssertTrue(
            source.contains("reduceMotion ? nil : .easeOut(duration: V4.hoverDuration)"),
            "the hover animation no longer branches on Reduce Motion")
        XCTAssertFalse(
            source.contains("hovering && !reduceMotion"),
            "Reduce Motion must not suppress the hover FILL, only its transition")
    }

    /// The fill has to match the control's shape rather than square off inside
    /// it, so each call site passes its own radius.
    func testEveryCallSitePassesItsOwnCornerRadius() throws {
        let expected: [(file: String, radius: String)] = [
            ("V4Button.swift", "V4.buttonRadius"),
            ("V4Button.swift", "V4.discRadius"),
            ("SegmentedTabs.swift", "V4.tabRadius"),
            ("PanelHeader.swift", "V4.gearRadius"),
        ]
        for (file, radius) in expected {
            let source = try panelV4Source(file)
            XCTAssertTrue(
                source.contains("V4PressStyle(cornerRadius: \(radius))"),
                "\(file) no longer passes \(radius) to V4PressStyle — its hover "
                    + "fill will draw at the button radius instead of its own")
        }
    }

    /// A bare `V4PressStyle()` anywhere means a control whose hover draws at
    /// the wrong radius, silently.
    func testNoCallSiteTakesTheDefaultRadiusByAccident() throws {
        for file in ["V4Button.swift", "SegmentedTabs.swift", "PanelHeader.swift"] {
            let source = try panelV4Source(file)
            XCTAssertFalse(
                source.contains(".buttonStyle(V4PressStyle())"),
                "\(file) applies V4PressStyle with no radius")
        }
    }

    /// The two numbers are the sheet's, and they live in `V4.swift` like every
    /// other number on this panel — `scripts/check-panel-v4.sh` refuses them
    /// anywhere else.
    func testTheHoverNumbersAreInTheSheet() throws {
        let sheet = try panelV4Source("V4.swift")
        XCTAssertTrue(sheet.contains("static let hoverFillAlpha: Double = 0.07"))
        XCTAssertTrue(sheet.contains("static let hoverDuration: Double = 0.15"))
    }

    private func panelV4Source(_ name: String) throws -> String {
        let repoRoot = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent()  // -> TcrBarTests
            .deletingLastPathComponent()  // -> Tests
            .deletingLastPathComponent()  // -> apps/macos
            .deletingLastPathComponent()  // -> apps
            .deletingLastPathComponent()  // -> repo root
        let file = repoRoot.appendingPathComponent("apps/macos/Sources/TcrBar/PanelV4/\(name)")
        return try String(contentsOf: file, encoding: .utf8)
    }
}
