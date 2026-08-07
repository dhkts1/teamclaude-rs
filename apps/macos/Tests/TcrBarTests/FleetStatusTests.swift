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
        XCTAssertEqual(bob.soonestHold?.minutesUntilReset, 93)
        XCTAssertEqual(bob.held[0].minutesUntilReset, 1528)
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

    func testWorstAccountIsStillReportable() throws {
        let fleet = try fleet(liveFixture)
        XCTAssertEqual(fleet.worst?.name, "bob@example.com")
    }

    func testDisabledAccountsAreNotEnabled() throws {
        let fleet = try fleet(offlineUnknownFixture)
        XCTAssertTrue(fleet.enabledAccounts.isEmpty)
    }

    func testEmptyArrayDecodes() throws {
        let fleet = try fleet("[]")
        XCTAssertTrue(fleet.accounts.isEmpty)
        XCTAssertEqual(fleet.source, .unknown("none"))
    }
}

/// Hand-built accounts for the capacity aggregates. Only the fields the
/// aggregates read carry meaning; the rest are inert. Names stay obviously fake.
private func account(
    _ name: String,
    state: QuotaState,
    disabled: Bool = false,
    held: [HeldWindow] = []
) -> Account {
    Account(
        name: name,
        priority: 1,
        status: "active",
        disabled: disabled,
        quota: 0,
        quotaState: state,
        fiveHour: 0,
        sevenDay: 0,
        sevenDayOi: 0,
        held: held,
        requests: 0,
        inputTokens: 0,
        outputTokens: 0,
        cacheReadTokens: 0,
        cacheHitRatio: nil,
        probeStatus: .ok,
        probeError: nil,
        lastStreamError: nil,
        streamErrorCount: 0,
        source: .live,
        serverSha: "abc1234",
        serverDirty: false
    )
}

final class FleetCapacitySummaryTests: XCTestCase {
    func testHealthyFleetReadsAsAllOk() {
        let fleet = Fleet(accounts: (1...12).map { account("a\($0)@example.com", state: .ok) })
        XCTAssertEqual(fleet.readyCount, 12)
        XCTAssertEqual(fleet.capacitySummary, "12 of 12 ready")
        XCTAssertEqual(fleet.capacityState, .ok)
        // Zero buckets are omitted, so a healthy fleet is a single chip.
        XCTAssertEqual(fleet.breakdownLabel, "12 ok")
    }

    func testMixedFleetCountsEachBucketOnce() {
        // Shape of a real fleet: 13 accounts, one operator-disabled.
        var accounts = (1...4).map { account("ok\($0)@example.com", state: .ok) }
        accounts.append(
            account(
                "near1@example.com",
                state: .near,
                held: [HeldWindow(window: "5h", minutesUntilReset: 168, resetAtMs: 1)]
            )
        )
        accounts += (1...7).map {
            account(
                "spent\($0)@example.com",
                state: .spent,
                held: [HeldWindow(window: "7d", minutesUntilReset: 1528, resetAtMs: 2)]
            )
        }
        // Disabled but nominally `ok`: it must not be counted as capacity.
        accounts.append(account("off1@example.com", state: .ok, disabled: true))
        let fleet = Fleet(accounts: accounts)

        XCTAssertEqual(fleet.accounts.count, 13)
        XCTAssertEqual(fleet.enabledCount, 12)
        XCTAssertEqual(fleet.readyCount, 4)
        XCTAssertEqual(fleet.capacitySummary, "4 of 12 ready")
        XCTAssertEqual(fleet.breakdownLabel, "4 ok · 1 near · 7 spent · 1 disabled")
        XCTAssertEqual(fleet.breakdown.map(\.count).reduce(0, +), fleet.accounts.count)
        XCTAssertEqual(fleet.soonestRecovery?.minutesUntilReset, 168)
    }

    func testDisabledAccountsAreNeverCapacity() {
        let fleet = Fleet(accounts: [
            account("on1@example.com", state: .ok),
            account("off1@example.com", state: .ok, disabled: true),
            account("off2@example.com", state: .ok, disabled: true),
        ])
        XCTAssertEqual(fleet.readyCount, 1, "a disabled account reports ok but serves nothing")
        XCTAssertEqual(fleet.enabledCount, 1)
        XCTAssertEqual(fleet.capacitySummary, "1 of 1 ready")
        XCTAssertEqual(fleet.breakdownLabel, "1 ok · 2 disabled")
    }

    func testZeroReadyFleetShowsTheSoonestRecovery() {
        let fleet = Fleet(accounts: [
            account(
                "spent1@example.com",
                state: .spent,
                held: [HeldWindow(window: "7d", minutesUntilReset: 1528, resetAtMs: 1)]
            ),
            account(
                "spent2@example.com",
                state: .spent,
                held: [
                    HeldWindow(window: "7d", minutesUntilReset: 900, resetAtMs: 2),
                    HeldWindow(window: "5h", minutesUntilReset: 168, resetAtMs: 3),
                ]
            ),
            // Disabled and resetting sooner — not capacity, so not the answer.
            account(
                "off1@example.com",
                state: .spent,
                disabled: true,
                held: [HeldWindow(window: "5h", minutesUntilReset: 5, resetAtMs: 4)]
            ),
        ])
        XCTAssertEqual(fleet.readyCount, 0)
        XCTAssertEqual(fleet.capacityState, .spent)
        XCTAssertEqual(fleet.soonestRecovery?.minutesUntilReset, 168)
        XCTAssertEqual(fleet.capacitySummary, "No capacity · next in 2h 48m")
    }

    func testZeroReadyWithNothingHeldStillSaysSomething() {
        // No held window means we cannot promise a time — say so, never "next in now".
        let fleet = Fleet(accounts: [account("weird1@example.com", state: .unknown("brand-new"))])
        XCTAssertEqual(fleet.readyCount, 0)
        XCTAssertEqual(fleet.capacitySummary, "No capacity")
        XCTAssertEqual(fleet.breakdownLabel, "1 unknown")
    }

    func testEmptyFleetIsNotZeroCapacity() {
        let fleet = Fleet(accounts: [])
        XCTAssertEqual(fleet.readyCount, 0)
        XCTAssertEqual(fleet.enabledCount, 0)
        XCTAssertEqual(fleet.capacitySummary, "No enabled accounts")
        XCTAssertEqual(fleet.capacityState, .unknown("empty"))
        XCTAssertTrue(fleet.breakdown.isEmpty)
        XCTAssertEqual(fleet.breakdownLabel, "")
    }
}

/// Reset-time rendering. Every assertion injects a fixed reference date, a fixed
/// calendar and a fixed locale — nothing here reads the wall clock or the
/// machine's region, so the suite means the same thing on any machine at any
/// hour. `en_GB` is a 24-hour locale and `en_US` a 12-hour one; using both is how
/// the "the clock format comes from the locale" claim is actually checked rather
/// than asserted.
final class HeldWindowResetTimeTests: XCTestCase {
    private let utc = TimeZone(identifier: "UTC")!
    private let gb = Locale(identifier: "en_GB")

    private var calendar: Calendar {
        var cal = Calendar(identifier: .gregorian)
        cal.timeZone = utc
        return cal
    }

    private func date(_ y: Int, _ m: Int, _ d: Int, _ hour: Int, _ minute: Int) -> Date {
        var components = DateComponents()
        components.year = y
        components.month = m
        components.day = d
        components.hour = hour
        components.minute = minute
        guard let date = calendar.date(from: components) else {
            preconditionFailure("fixed test date must be constructible")
        }
        return date
    }

    private func absolute(_ reset: Date, now: Date, locale: Locale? = nil) -> String {
        HeldWindow.absoluteResetLabel(
            resetAt: reset,
            now: now,
            calendar: calendar,
            locale: locale ?? gb,
            timeZone: utc
        )
    }

    // 2026-08-06 is a Thursday; 2026-08-10 is the following Monday.
    private var now: Date { date(2026, 8, 6, 9, 0) }

    func testDayTierReplacesAThreeDigitHourCount() {
        // The live case that motivated the tier: 6526 minutes rendered "108h 46m".
        XCTAssertEqual(HeldWindow.duration(minutes: 6526), "4d 12h")
        XCTAssertEqual(HeldWindow.duration(minutes: 1440), "1d")
        XCTAssertEqual(HeldWindow.duration(minutes: 1528), "1d 1h")
        XCTAssertEqual(HeldWindow.duration(minutes: 10080), "7d")
    }

    func testSubDayTiersAreUnchanged() {
        XCTAssertEqual(HeldWindow.duration(minutes: 0), "now")
        XCTAssertEqual(HeldWindow.duration(minutes: -5), "now")
        XCTAssertEqual(HeldWindow.duration(minutes: 45), "45m")
        XCTAssertEqual(HeldWindow.duration(minutes: 120), "2h")
        XCTAssertEqual(HeldWindow.duration(minutes: 166), "2h 46m")
        XCTAssertEqual(HeldWindow.duration(minutes: 1439), "23h 59m")
    }

    func testTodayIsJustAClockTime() {
        XCTAssertEqual(absolute(date(2026, 8, 6, 17, 0), now: now), "17:00")
    }

    func testTomorrowIsNamed() {
        XCTAssertEqual(absolute(date(2026, 8, 7, 23, 0), now: now), "tomorrow 23:00")
    }

    func testBeyondTomorrowIsAWeekday() {
        XCTAssertEqual(absolute(date(2026, 8, 10, 15, 0), now: now), "Mon 15:00")
    }

    func testEarlierTodayIsStillToday() {
        // Same calendar day but already past: proximity is by day, not by sign.
        XCTAssertEqual(absolute(date(2026, 8, 6, 1, 30), now: now), "01:30")
    }

    func testClockFormatFollowsTheLocaleRatherThanAHardcodedPattern() {
        let american = absolute(date(2026, 8, 6, 15, 0), now: now, locale: Locale(identifier: "en_US"))
        XCTAssertTrue(
            american.contains("PM"),
            "a 12-hour locale must get a 12-hour clock, got \(american)"
        )
        XCTAssertTrue(american.hasPrefix("3:"), "got \(american)")
        // The same instant in a 24-hour locale.
        XCTAssertEqual(absolute(date(2026, 8, 6, 15, 0), now: now), "15:00")
    }

    func testFullLabelCarriesBothWhenAndHowLong() {
        // 6526 minutes after 2026-08-06 09:00 UTC is 2026-08-10 21:46 UTC.
        let reset = date(2026, 8, 10, 21, 46)
        let hold = HeldWindow(
            window: "7d",
            minutesUntilReset: 6526,
            resetAtMs: Int64(reset.timeIntervalSince1970 * 1000)
        )
        XCTAssertEqual(
            hold.label(now: now, calendar: calendar, locale: gb, timeZone: utc),
            "7d · resets Mon 21:46 · in 4d 12h"
        )
    }

    func testWindowLabelIsRenderedVerbatimNotSpecialCased() {
        // Only `7d` windows exist in the live fleet today. Nothing may branch on
        // the string, so an unfamiliar window still renders.
        let reset = date(2026, 8, 6, 11, 46)
        for window in ["5h", "7d", "weekly-opus", ""] {
            let hold = HeldWindow(
                window: window,
                minutesUntilReset: 166,
                resetAtMs: Int64(reset.timeIntervalSince1970 * 1000)
            )
            XCTAssertEqual(
                hold.label(now: now, calendar: calendar, locale: gb, timeZone: utc),
                "\(window) · resets 11:46 · in 2h 46m"
            )
        }
    }

    func testMissingTimestampDropsTheAbsoluteHalfRatherThanInventingOne() {
        let hold = HeldWindow(window: "7d", minutesUntilReset: 166, resetAtMs: 0)
        XCTAssertEqual(
            hold.label(now: now, calendar: calendar, locale: gb, timeZone: utc),
            "7d · in 2h 46m"
        )
    }
}

/// The menu-bar glyph. Worst-account-wins made the glyph read `spent` whenever
/// any one of thirteen accounts was spent — which, in a rotating pool, is the
/// normal condition. These pin the replacement mapping.
final class FleetCapacityGlyphTests: XCTestCase {
    private func held(_ minutes: Int) -> [HeldWindow] {
        [HeldWindow(window: "7d", minutesUntilReset: minutes, resetAtMs: 1)]
    }

    func testAnyReadyAccountIsGreenEvenBesideSpentOnes() {
        let fleet = Fleet(accounts: [
            account("ok1@example.com", state: .ok),
            account("near1@example.com", state: .near, held: held(166)),
        ] + (1...11).map { account("spent\($0)@example.com", state: .spent, held: held(6526)) })
        XCTAssertEqual(fleet.readyCount, 1)
        XCTAssertEqual(fleet.capacityGlyphState, .ok, "one ready account is capacity")
    }

    func testNoneReadyButSomethingNearIsAmber() {
        let fleet = Fleet(accounts: [
            account("spent1@example.com", state: .spent, held: held(6526)),
            account("near1@example.com", state: .near, held: held(166)),
        ])
        XCTAssertEqual(fleet.readyCount, 0)
        XCTAssertEqual(fleet.capacityGlyphState, .near)
    }

    func testNoneReadyAndNoneNearIsRed() {
        let fleet = Fleet(accounts: (1...3).map {
            account("spent\($0)@example.com", state: .spent, held: held(6526))
        })
        XCTAssertEqual(fleet.capacityGlyphState, .spent)
    }

    func testADisabledReadyAccountIsNotCapacity() {
        // The trap the whole glyph change is about: `disabled` accounts keep
        // reporting `ok` while serving nothing.
        let fleet = Fleet(accounts: [
            account("off1@example.com", state: .ok, disabled: true),
            account("spent1@example.com", state: .spent, held: held(6526)),
        ])
        XCTAssertEqual(fleet.capacityGlyphState, .spent)
    }

    func testADisabledNearAccountDoesNotSoftenTheGlyph() {
        // A disabled account is out of rotation whatever its quota says, so its
        // `near` must not downgrade a genuinely spent pool to amber.
        let fleet = Fleet(accounts: [
            account("off1@example.com", state: .near, disabled: true, held: held(166)),
            account("spent1@example.com", state: .spent, held: held(6526)),
        ])
        XCTAssertEqual(fleet.capacityGlyphState, .spent)
    }

    func testAnAllDisabledFleetIsUnknownNotAnAlarm() {
        let fleet = Fleet(accounts: (1...3).map {
            account("off\($0)@example.com", state: .ok, disabled: true)
        })
        XCTAssertTrue(fleet.enabledAccounts.isEmpty)
        XCTAssertEqual(fleet.capacityGlyphState, .unknown("empty"))
        XCTAssertEqual(fleet.capacityGlyphState, fleet.capacityState, "the two agree on an empty fleet")
    }

    func testAnEmptyFleetIsUnknown() {
        XCTAssertEqual(Fleet(accounts: []).capacityGlyphState, .unknown("empty"))
    }
}

/// Enable/disable. Only the argument vector and the exit classification are
/// covered: running the real command would mutate the operator's live config, so
/// nothing here executes anything.
final class AccountCommandTests: XCTestCase {
    func testArgumentsPassTheExactNamePositionally() {
        XCTAssertEqual(
            AccountCommand.arguments(enabled: false, name: "alice@example.com"),
            ["disable", "alice@example.com"]
        )
        XCTAssertEqual(
            AccountCommand.arguments(enabled: true, name: "alice@example.com"),
            ["enable", "alice@example.com"]
        )
    }

    func testTheNameIsNeverTruncatedOrFlagged() {
        // `query` is a case-insensitive substring match on the Rust side, so an
        // abbreviated name could silently hit a different account. Pass it whole.
        let name = "alice+tag@example.com"
        let arguments = AccountCommand.arguments(enabled: false, name: name)
        XCTAssertEqual(arguments.count, 2, "no flags, no --org, nothing else")
        XCTAssertEqual(arguments[1], name)
        XCTAssertFalse(arguments.contains { $0.hasPrefix("--") })
    }

    func testSubcommandIsTheOnlyThingThatVaries() {
        let name = "bob@example.com"
        let enable = AccountCommand.arguments(enabled: true, name: name)
        let disable = AccountCommand.arguments(enabled: false, name: name)
        XCTAssertEqual(Array(enable.dropFirst()), Array(disable.dropFirst()))
        XCTAssertEqual(Set([enable[0], disable[0]]), ["enable", "disable"])
    }

    func testExitZeroIsSuccess() {
        XCTAssertNil(AccountCommand.classify(enabling: true, exitCode: 0, stderr: ""))
        XCTAssertNil(
            AccountCommand.classify(enabling: false, exitCode: 0, stderr: "some chatter"),
            "exit code decides, not stderr chatter"
        )
    }

    func testAnAmbiguousQueryIsSurfacedVerbatim() {
        // The realistic failure: one configured name is a substring of another.
        let failure = AccountCommand.classify(
            enabling: false,
            exitCode: 1,
            stderr: "  no unique account matches \"alice\" (2 candidates)\n"
        )
        XCTAssertEqual(failure?.exitCode, 1)
        XCTAssertEqual(failure?.message, "no unique account matches \"alice\" (2 candidates)")
        XCTAssertEqual(
            failure?.summary,
            "disable failed (exit 1): no unique account matches \"alice\" (2 candidates)"
        )
    }

    func testASilentFailureStillSaysSomething() {
        let failure = AccountCommand.classify(enabling: true, exitCode: 2, stderr: "")
        XCTAssertEqual(failure?.summary, "enable failed (exit 2): no output")
        XCTAssertFalse(failure?.summary.isEmpty ?? true)
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
    func testDefaultSpawnUsesNoReplace() {
        // The safety property: replacing an incumbent proxy wipes the session pin
        // map and costs every live session a cold prompt-cache prefix. Every
        // routine start must therefore refuse to do it.
        XCTAssertTrue(ServerController.safeArguments.contains("--no-replace"))
        XCTAssertEqual(ServerController.serverArguments, ServerController.safeArguments)
    }

    func testTakeoverOmitsNoReplace() {
        // The takeover is expressed *only* by dropping `--no-replace`: tcr's own
        // singleton then does the replacing.
        XCTAssertFalse(ServerController.takeoverArguments.contains("--no-replace"))
    }

    /// The regression that made every spawn path dead on arrival.
    ///
    /// Without `--headless`, `tcr server` runs its ratatui TUI
    /// (`src/main.rs:590`) which calls `enable_raw_mode()?` on stdout
    /// (`src/tui.rs:47`). A GUI spawns with a `Pipe`, so there is no terminal, raw
    /// mode fails and the child exits at once — the server appeared for an instant
    /// and vanished.
    ///
    /// The previous version of these tests asserted the exact argument arrays and
    /// passed for the entire time the feature was broken, because pinning a
    /// literal list says nothing about whether the list works. This asserts the
    /// property that actually matters.
    func testEverySpawnPathIsHeadless() {
        for arguments in [ServerController.safeArguments, ServerController.takeoverArguments] {
            XCTAssertTrue(
                arguments.contains("--headless"),
                "\(arguments) would start a TUI with no terminal and die on launch"
            )
        }
    }

    func testNeitherArgumentSetCarriesAnythingUnexpected() {
        let known: Set<String> = ["server", "--no-replace", "--headless"]
        for arguments in [ServerController.safeArguments, ServerController.takeoverArguments] {
            XCTAssertEqual(arguments.first, "server", "both sets are `tcr server`")
            XCTAssertTrue(
                Set(arguments).isSubset(of: known),
                "unexpected argument in \(arguments) — the two sets are exhaustive by design"
            )
            XCTAssertFalse(
                arguments.contains { $0.contains("kill") || $0.contains("pid") },
                "\(arguments) must never name a process"
            )
        }
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

    func testAnyhowBindContextIsTreatedAsAlreadyRunning() {
        // The real first line tcr prints on errno 48: `failed to bind` is our own
        // anyhow context, and it arrives *before* the OS strerror in the cause
        // chain — so a truncated read that only ever sees the first line must
        // still classify as "a server is up".
        let state = ServerController.classifyExit(
            exitCode: 1,
            stderr: "Error: failed to bind 127.0.0.1:3456"
        )
        guard case .incumbentHoldsPort = state else {
            return XCTFail("tcr's own bind-failure wording means the port is taken")
        }
        XCTAssertFalse(state.isOurChild)
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

/// Only the pure mapping is covered. `SMAppService.mainApp.register()` /
/// `unregister()` mutate real per-user launch state and `NSAlert` needs a run
/// loop and a human, so neither is exercised here — see the report accompanying
/// this change for what that leaves uncovered.
final class LoginItemStatusTests: XCTestCase {
    func testEveryServiceStatusMapsToADistinctCase() {
        XCTAssertEqual(LoginItem.classify(.enabled), .enabled)
        XCTAssertEqual(LoginItem.classify(.notRegistered), .disabled)
        XCTAssertEqual(LoginItem.classify(.requiresApproval), .requiresApproval)
        XCTAssertEqual(LoginItem.classify(.notFound), .notFound)
    }

    func testApprovalPendingIsNotDrawnAsOff() {
        // The dishonest option is showing an unapproved registration as a plain
        // "off": the operator did ask for it, and macOS is waiting on them.
        XCTAssertTrue(LoginItem.Status.requiresApproval.isOn)
        let detail = LoginItem.Status.requiresApproval.detail
        XCTAssertNotNil(detail)
        XCTAssertTrue(detail?.contains("System Settings") == true, "must say where to go")
    }

    func testSettledStatesSayNothingExtra() {
        XCTAssertNil(LoginItem.Status.enabled.detail)
        XCTAssertNil(LoginItem.Status.disabled.detail)
        XCTAssertTrue(LoginItem.Status.enabled.isOn)
        XCTAssertFalse(LoginItem.Status.disabled.isOn)
    }

    func testUnclassifiableStatesAreVisibleAndOff() {
        for status: LoginItem.Status in [.notFound, .unrecognised(rawValue: 99)] {
            XCTAssertFalse(status.isOn, "\(status) must not read as enabled")
            XCTAssertNotNil(status.detail, "\(status) must explain itself")
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
        let bundle = URL(fileURLWithPath: "/nonexistent-bundle", isDirectory: true)
        let result = TcrTool.resolve(
            environment: ["PATH": "/nonexistent-dir"],
            defaults: UserDefaults(suiteName: "com.github.dhkts1.tcrbar.tests")!,
            home: URL(fileURLWithPath: "/nonexistent-home", isDirectory: true),
            bundle: bundle
        )
        guard case .failure(let notFound) = result else {
            return XCTFail("no tcr should have been found under a fake PATH")
        }
        XCTAssertFalse(notFound.searched.isEmpty, "the error must name where it looked")
        // The bundled candidate is a real place this looked, so a truthful
        // "not found" has to name it too.
        XCTAssertTrue(
            notFound.searched.contains("/nonexistent-bundle/tcr"),
            "the searched list must include the bundle candidate, got \(notFound.searched)"
        )
    }

    /// Writes an executable `tcr` into a fresh temp directory and returns it.
    private func makeToolDirectory() throws -> URL {
        let dir = URL(fileURLWithPath: NSTemporaryDirectory(), isDirectory: true)
            .appendingPathComponent("tcrtool-\(UUID().uuidString)", isDirectory: true)
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        addTeardownBlock { try? FileManager.default.removeItem(at: dir) }
        let tool = dir.appendingPathComponent("tcr")
        try "#!/bin/sh\nexit 0\n".write(to: tool, atomically: true, encoding: .utf8)
        try FileManager.default.setAttributes([.posixPermissions: 0o755], ofItemAtPath: tool.path)
        return tool
    }

    private var scratchDefaults: UserDefaults {
        let defaults = UserDefaults(suiteName: "com.github.dhkts1.tcrbar.tests")!
        defaults.removeObject(forKey: TcrTool.overrideDefaultsKey)
        return defaults
    }

    func testBundledBinaryBeatsPath() throws {
        let bundled = try makeToolDirectory()
        let onPath = try makeToolDirectory()
        let result = TcrTool.resolve(
            environment: ["PATH": onPath.deletingLastPathComponent().path],
            defaults: scratchDefaults,
            home: URL(fileURLWithPath: "/nonexistent-home", isDirectory: true),
            bundle: bundled.deletingLastPathComponent()
        )
        guard case .success(let found) = result else {
            return XCTFail("a tcr exists in both the bundle and on PATH, got \(result)")
        }
        XCTAssertEqual(
            found.path, bundled.path,
            "the bundled tcr must win over the one on PATH (\(onPath.path))"
        )
    }

    func testEnvironmentOverrideBeatsBundledBinary() throws {
        let bundled = try makeToolDirectory()
        let override = try makeToolDirectory()
        let result = TcrTool.resolve(
            environment: [TcrTool.overrideEnvKey: override.path, "PATH": "/nonexistent-dir"],
            defaults: scratchDefaults,
            home: URL(fileURLWithPath: "/nonexistent-home", isDirectory: true),
            bundle: bundled.deletingLastPathComponent()
        )
        guard case .success(let found) = result else {
            return XCTFail("TCR_BIN names a real executable, got \(result)")
        }
        XCTAssertEqual(
            found.path, override.path,
            "an explicit TCR_BIN must still beat the bundled tcr (\(bundled.path))"
        )
    }

    func testDefaultsOverrideBeatsBundledBinary() throws {
        let bundled = try makeToolDirectory()
        let override = try makeToolDirectory()
        let defaults = scratchDefaults
        defaults.set(override.path, forKey: TcrTool.overrideDefaultsKey)
        defer { defaults.removeObject(forKey: TcrTool.overrideDefaultsKey) }
        let result = TcrTool.resolve(
            environment: ["PATH": "/nonexistent-dir"],
            defaults: defaults,
            home: URL(fileURLWithPath: "/nonexistent-home", isDirectory: true),
            bundle: bundled.deletingLastPathComponent()
        )
        guard case .success(let found) = result else {
            return XCTFail("the defaults key names a real executable, got \(result)")
        }
        XCTAssertEqual(
            found.path, override.path,
            "an explicit defaults override must still beat the bundled tcr (\(bundled.path))"
        )
    }
}
