import XCTest

@testable import TcrBarCore

/// The table gate 1 of `data/plans/settings-window-bridge.md` asks for: every
/// row the Settings window draws carries an applied-live / restart-to-apply /
/// read-only marking, and none of them silently falls back to "no entry means
/// live" — that fallback is exactly the review's S3 finding
/// (`docs/design/panel-tabs-review.md`), reproduced in code instead of markup.
final class SettingsRowBadgeTests: XCTestCase {

    /// Every public key `SettingsRowBadge` declares must have a timing entry.
    /// A key present without an entry is the S3 trap: a row on screen with no
    /// badge, which the mockup review found reads as "applied live" by
    /// default rather than by any actual guarantee.
    func testEveryDeclaredKeyHasATimingEntry() {
        let declaredKeys: [String] = [
            SettingsRowBadge.proxyRestart, SettingsRowBadge.startServerAtLaunch,
            SettingsRowBadge.pollInterval, SettingsRowBadge.launchAtLogin,
            SettingsRowBadge.keepAwake, SettingsRowBadge.showReadyCount,
            SettingsRowBadge.showRunningToolCount, SettingsRowBadge.openOnTab,
            SettingsRowBadge.textSize, SettingsRowBadge.groupParked,
            SettingsRowBadge.groupReserved, SettingsRowBadge.groupMayServeAsControl,
            SettingsRowBadge.groupColor, SettingsRowBadge.groupMembers,
            SettingsRowBadge.switchThreshold, SettingsRowBadge.controlReserve,
            SettingsRowBadge.fableWeeklyThreshold, SettingsRowBadge.resetUrgencyTier,
            SettingsRowBadge.sessionAffinity, SettingsRowBadge.controlAccount,
            SettingsRowBadge.controlPooled, SettingsRowBadge.accountThrottle,
            SettingsRowBadge.fleetThrottle, SettingsRowBadge.pacing,
            SettingsRowBadge.usageRetentionDays, SettingsRowBadge.http1Only,
            SettingsRowBadge.checkNow, SettingsRowBadge.whatsNew,
            SettingsRowBadge.checkAutomatically, SettingsRowBadge.runningServer,
            SettingsRowBadge.installedCli, SettingsRowBadge.updatingReplacesBoth,
        ]
        for key in declaredKeys {
            XCTAssertNotNil(
                SettingsRowBadge.timing(for: key), "\(key) has no timing entry in the table")
        }
        // The reverse direction too: nothing in the table names a key this
        // test does not know about, so an entry added to the dictionary
        // without a matching `public static let` cannot go unreviewed.
        XCTAssertEqual(Set(declaredKeys), Set(SettingsRowBadge.timing.keys))
    }

    /// The five keys the review's S3 finding named explicitly — the LIMITS
    /// section that sat under the one section marked "restart to apply" and
    /// silently inherited nothing — are ALL `.boot`, matching
    /// `docs/configuration.md`'s "every other config field is a boot-time
    /// snapshot" and never `.live`.
    func testEveryLimitsRowIsBootTime() {
        for key in [
            SettingsRowBadge.accountThrottle, SettingsRowBadge.fleetThrottle,
            SettingsRowBadge.pacing, SettingsRowBadge.usageRetentionDays,
            SettingsRowBadge.http1Only,
        ] {
            XCTAssertEqual(SettingsRowBadge.timing(for: key), .boot)
        }
    }

    /// A key this app has no write path for at all — `GroupController` covers
    /// park, not reserve or control-eligibility — must read `.readOnly`, not
    /// `.boot`: `.boot` would imply a control this window does not offer.
    func testAKeyWithNoWritePathIsReadOnlyNotBoot() {
        XCTAssertEqual(SettingsRowBadge.timing(for: SettingsRowBadge.groupReserved), .readOnly)
        XCTAssertEqual(
            SettingsRowBadge.timing(for: SettingsRowBadge.groupMayServeAsControl), .readOnly)
    }

    func testAnUnknownKeyHasNoEntry() {
        XCTAssertNil(SettingsRowBadge.timing(for: "not.a.real.key"))
    }

    func testLabelsMatchTheMockupsTwoWordTags() {
        XCTAssertEqual(SettingsRowTiming.live.label, "applied live")
        XCTAssertEqual(SettingsRowTiming.boot.label, "restart to apply")
        XCTAssertEqual(SettingsRowTiming.readOnly.label, "read-only")
    }
}
