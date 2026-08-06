import XCTest

@testable import TcrBarCore

/// Fixtures use obviously-fake account names only. Real account emails never
/// enter this repository — see CLAUDE.md.
private let liveFixture = """
[
  {
    "source": "live",
    "serverSha": "abc1234",
    "serverDirty": false,
    "name": "alice@example.com",
    "priority": 1,
    "status": "active",
    "disabled": false,
    "quota": 0.42,
    "quotaState": "ok",
    "fiveHour": 0.11,
    "sevenDay": 0.42,
    "sevenDayOi": 0.05,
    "held": [],
    "requests": 128,
    "inputTokens": 4096,
    "outputTokens": 512,
    "cacheReadTokens": 2048,
    "cacheHitRatio": 0.5,
    "probeStatus": "ok",
    "probeError": null,
    "lastStreamError": null,
    "streamErrorCount": 0
  },
  {
    "source": "live",
    "serverSha": "abc1234",
    "serverDirty": false,
    "name": "bob@example.com",
    "priority": 2,
    "status": "held",
    "disabled": false,
    "quota": 1.0,
    "quotaState": "spent",
    "fiveHour": 1.0,
    "sevenDay": 1.0,
    "sevenDayOi": 0.9,
    "held": [
      {"window": "7d", "minutesUntilReset": 1528, "resetAtMs": 1000000000000},
      {"window": "5h", "minutesUntilReset": 93, "resetAtMs": 999000000000}
    ],
    "requests": 0,
    "inputTokens": 0,
    "outputTokens": 0,
    "cacheReadTokens": 0,
    "cacheHitRatio": null,
    "probeStatus": "ok",
    "probeError": null,
    "lastStreamError": "overloaded_error",
    "streamErrorCount": 3
  }
]
"""

/// Same shape, `source: offline` and a `quotaState` this build has never seen.
private let offlineUnknownFixture = """
[
  {
    "source": "offline",
    "serverSha": null,
    "serverDirty": null,
    "name": "carol@example.com",
    "priority": 3,
    "status": "active",
    "disabled": true,
    "quota": 0.0,
    "quotaState": "brand-new-variant",
    "fiveHour": 0.0,
    "sevenDay": 0.0,
    "sevenDayOi": 0.0,
    "held": [],
    "requests": 0,
    "inputTokens": 0,
    "outputTokens": 0,
    "cacheReadTokens": 0,
    "cacheHitRatio": null,
    "probeStatus": "skipped",
    "probeError": "no server",
    "lastStreamError": null,
    "streamErrorCount": 0
  }
]
"""

final class FleetStatusTests: XCTestCase {
    private func fleet(_ json: String) throws -> Fleet {
        try Fleet.decode(Data(json.utf8))
    }

    func testDecodesLiveFleet() throws {
        let fleet = try fleet(liveFixture)
        XCTAssertEqual(fleet.accounts.count, 2)
        XCTAssertEqual(fleet.accounts[0].name, "alice@example.com")
        XCTAssertEqual(fleet.accounts[0].quotaState, .ok)
        XCTAssertEqual(fleet.accounts[0].cacheHitRatio, 0.5)
        XCTAssertEqual(fleet.source, .live)
        XCTAssertEqual(fleet.serverSha, "abc1234")
        XCTAssertFalse(fleet.source.countersAreStructural)
    }

    func testNullCacheRatioStaysNilRatherThanZero() throws {
        let fleet = try fleet(liveFixture)
        // The honesty case: a null ratio must never decode to a measured 0.0.
        XCTAssertNil(fleet.accounts[1].cacheHitRatio)
    }

    func testHeldWindowsAndSoonestHold() throws {
        let fleet = try fleet(liveFixture)
        let bob = fleet.accounts[1]
        XCTAssertEqual(bob.held.count, 2)
        XCTAssertEqual(bob.soonestHold?.window, "5h")
        XCTAssertEqual(bob.soonestHold?.countdownLabel, "5h resets in 1h 33m")
        XCTAssertEqual(bob.held[0].countdownLabel, "7d resets in 25h 28m")
    }

    func testDurationFormatting() {
        XCTAssertEqual(HeldWindow.duration(minutes: 0), "now")
        XCTAssertEqual(HeldWindow.duration(minutes: -5), "now")
        XCTAssertEqual(HeldWindow.duration(minutes: 45), "45m")
        XCTAssertEqual(HeldWindow.duration(minutes: 120), "2h")
        XCTAssertEqual(HeldWindow.duration(minutes: 1528), "25h 28m")
    }

    func testUnknownQuotaStateDegradesInsteadOfThrowing() throws {
        let fleet = try fleet(offlineUnknownFixture)
        XCTAssertEqual(fleet.accounts[0].quotaState, .unknown("brand-new-variant"))
        XCTAssertEqual(fleet.accounts[0].quotaState.token, "brand-new-variant")
    }

    func testOfflineSourceIsFlaggedAsStructural() throws {
        let fleet = try fleet(offlineUnknownFixture)
        XCTAssertEqual(fleet.source, .offline)
        XCTAssertTrue(fleet.source.countersAreStructural)
        XCTAssertNil(fleet.serverSha)
        XCTAssertFalse(fleet.serverDirty)
    }

    func testHeadlineIsWorstEnabledAccount() throws {
        let fleet = try fleet(liveFixture)
        XCTAssertEqual(fleet.headline, .spent)
        XCTAssertEqual(fleet.worst?.name, "bob@example.com")
    }

    func testDisabledAccountsAreExcludedFromHeadline() throws {
        let fleet = try fleet(offlineUnknownFixture)
        XCTAssertTrue(fleet.enabledAccounts.isEmpty)
        XCTAssertEqual(fleet.headline, .unknown("empty"))
    }

    func testEmptyArrayDecodes() throws {
        let fleet = try fleet("[]")
        XCTAssertTrue(fleet.accounts.isEmpty)
        XCTAssertEqual(fleet.source, .unknown("none"))
    }
}

final class StatusPollerClassifyTests: XCTestCase {
    func testNonZeroExitIsReportedWithStderr() {
        let output = TcrTool.Output(exitCode: 1, stdout: Data(), stderr: "connection refused\n")
        XCTAssertEqual(
            StatusPoller.classify(output),
            .commandFailed(exitCode: 1, message: "connection refused")
        )
    }

    func testGarbageStdoutIsUndecodableNotEmpty() {
        let output = TcrTool.Output(exitCode: 0, stdout: Data("not json".utf8), stderr: "")
        guard case .undecodable = StatusPoller.classify(output) else {
            return XCTFail("garbage must not decode to an empty fleet")
        }
    }

    func testSummariesAreNeverEmpty() {
        let states: [PollState] = [
            .pending,
            .toolMissing(searched: ["a", "b"]),
            .commandFailed(exitCode: 2, message: ""),
            .undecodable(message: "boom"),
            .loaded(Fleet(accounts: [])),
        ]
        for state in states {
            XCTAssertFalse(state.summary.isEmpty, "\(state) rendered an empty summary")
        }
    }
}

final class ServerControllerTests: XCTestCase {
    func testAlwaysSpawnsWithNoReplace() {
        // The safety property: replacing an incumbent proxy wipes the session pin
        // map and costs every live session a cold prompt-cache prefix.
        XCTAssertEqual(ServerController.serverArguments, ["server", "--no-replace"])
        XCTAssertTrue(ServerController.serverArguments.contains("--no-replace"))
    }

    func testIncumbentIsSuccessNotFailure() {
        let stderr = "[tcr] another proxy holds :3456 (pid 123) and --no-replace was set; "
            + "two proxies refreshing the same single-use tokens will token-war. Not replacing."
        let state = ServerController.classifyExit(exitCode: 1, stderr: stderr)
        guard case .incumbentHoldsPort = state else {
            return XCTFail("an already-running server must not be reported as an error")
        }
        XCTAssertFalse(state.isOurChild, "an incumbent we did not spawn is never ours to signal")
    }

    func testBindFailureIsAlsoTreatedAsAlreadyRunning() {
        let state = ServerController.classifyExit(exitCode: 1, stderr: "Address already in use (os error 48)")
        guard case .incumbentHoldsPort = state else {
            return XCTFail("a taken port means a server is up")
        }
    }

    func testGenuineFailureIsSurfacedVerbatim() {
        let state = ServerController.classifyExit(exitCode: 101, stderr: "config parse error")
        XCTAssertEqual(state, .exited(exitCode: 101, message: "config parse error"))
        XCTAssertFalse(state.isOurChild)
    }

    func testOnlyASupervisedChildIsOurs() {
        XCTAssertTrue(ServerController.State.supervising(pid: 42).isOurChild)
        for state: ServerController.State in [
            .idle,
            .incumbentHoldsPort(message: "x"),
            .exited(exitCode: 0, message: ""),
            .toolMissing(searched: []),
        ] {
            XCTAssertFalse(state.isOurChild, "\(state) must never be stoppable by this app")
        }
    }
}

final class TcrToolTests: XCTestCase {
    func testSearchDirectoriesArePathFirstAndDeduplicated() {
        let home = URL(fileURLWithPath: "/nonexistent-home", isDirectory: true)
        let dirs = TcrTool.searchDirectories(
            environment: ["PATH": "/usr/bin:/opt/homebrew/bin"],
            home: home
        )
        XCTAssertEqual(dirs.first?.path, "/usr/bin")
        XCTAssertEqual(dirs.filter { $0.path == "/opt/homebrew/bin" }.count, 1)
        XCTAssertTrue(dirs.contains { $0.path.hasSuffix("/.local/bin") })
    }

    func testMissingToolReportsWhatItSearched() {
        let result = TcrTool.resolve(
            environment: ["PATH": "/nonexistent-dir"],
            defaults: UserDefaults(suiteName: "com.github.dhkts1.tcrbar.tests")!,
            home: URL(fileURLWithPath: "/nonexistent-home", isDirectory: true)
        )
        guard case .failure(let notFound) = result else {
            return XCTFail("no tcr should have been found under a fake PATH")
        }
        XCTAssertFalse(notFound.searched.isEmpty, "the error must name where it looked")
    }
}
