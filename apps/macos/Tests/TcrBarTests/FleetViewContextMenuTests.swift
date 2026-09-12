import XCTest

/// `AccountRow`'s `contextMenuItems` is a `@ViewBuilder` built from live
/// `AccountController`/`ControlAccountController`/`GroupController`/
/// `RemoveAccountController` dependencies with no lightweight test doubles in
/// this target, and ViewInspector is not a dependency here — so this reads
/// the source directly, the same technique `FleetStatusTests` already uses
/// (`testAccountStatusErrorTokenStillExistsInRustSource`) for an assertion a
/// runtime test can't reach cheaply.
final class ContextMenuReloginAvailabilityTests: XCTestCase {
    /// The bug this test guards: "Re-login…" used to disappear from the
    /// context menu the moment an account came back healthy, which made it
    /// useless as the migration path off the eight-hour credentials PR #214
    /// replaced with year-long ones — a healthy row is exactly the row an
    /// operator would want to migrate ahead of its old token's own expiry.
    func testReloginItemHasNoHealthGateInContextMenu() throws {
        let body = try contextMenuItemsBody()
        XCTAssertFalse(
            body.contains("account.health == .needsRelogin"),
            "contextMenuItems still gates \"Re-login…\" on .needsRelogin — "
                + "the item is hidden on every healthy row, so an account "
                + "that logged in before PR #214 has no way to pick up its "
                + "one-year credential without first breaking"
        )
        XCTAssertTrue(
            body.contains(#"Button("Re-login…") { onRelogin() }"#),
            "the \"Re-login…\" button itself has moved or been renamed in "
                + "contextMenuItems — update this test's anchor to match"
        )
    }

    /// Isolates the `contextMenuItems` computed property's source text so
    /// the assertion above cannot accidentally match some other `.needsRelogin`
    /// check elsewhere in the file (e.g. the standalone `reloginButton`'s own
    /// gate, which is deliberately left alone).
    private func contextMenuItemsBody() throws -> String {
        let thisFile = URL(fileURLWithPath: #filePath)
        let repoRoot =
            thisFile
            .deletingLastPathComponent()  // FleetViewContextMenuTests.swift -> TcrBarTests
            .deletingLastPathComponent()  // TcrBarTests -> Tests
            .deletingLastPathComponent()  // Tests -> apps/macos
            .deletingLastPathComponent()  // apps/macos -> apps
            .deletingLastPathComponent()  // apps -> repo root
        let fleetView = repoRoot.appendingPathComponent(
            "apps/macos/Sources/TcrBar/FleetView.swift")
        let contents = try String(contentsOf: fleetView, encoding: .utf8)
        guard
            let start = contents.range(
                of: "private var contextMenuItems: some View {"),
            let end = contents.range(
                of: "private var groupMenuItems: some View {",
                range: start.upperBound..<contents.endIndex)
        else {
            XCTFail(
                "contextMenuItems or groupMenuItems has moved or been renamed "
                    + "in \(fleetView.path) — update this test's anchors")
            return ""
        }
        return String(contents[start.upperBound..<end.lowerBound])
    }
}
