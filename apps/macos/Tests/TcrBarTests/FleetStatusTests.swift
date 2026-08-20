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

    /// The exact bug the divert-budget no-signal work exists to fix: an
    /// account that took real traffic (nonzero `requests`/`inputTokens`, so
    /// this is NOT the already-covered "zero traffic" case in
    /// ``testNullCacheRatioStaysNilRatherThanZero``) but not enough of it to
    /// trust a ratio. `cacheHitRatio: null` here is a low-sample account the
    /// Rust side has decided not to make a claim about — decoding it to
    /// anything other than `nil` would silently turn "not enough signal"
    /// back into "a measured percentage", which is precisely what a human
    /// glancing at the panel must not see.
    func testLowTrafficAccountWithNullRatioStaysNilNotZero() throws {
        let json = """
            [
              {
                "source": "live", "serverSha": "abc1234", "serverDirty": false,
                "name": "dana@example.com", "priority": 4, "status": "active",
                "disabled": false, "quota": 0.1, "quotaState": "ok",
                "fiveHour": 0.1, "sevenDay": 0.1, "sevenDayOi": 0.0, "held": [],
                "requests": 2, "inputTokens": 64, "outputTokens": 8,
                "cacheReadTokens": 1, "cacheHitRatio": null,
                "probeStatus": "ok", "probeError": null, "lastStreamError": null,
                "streamErrorCount": 0
              }
            ]
            """
        let decoded = try fleet(json)
        let dana = decoded.accounts[0]
        XCTAssertGreaterThan(
            dana.requests ?? 0, 0,
            "this fixture must exercise real traffic, not the zero-traffic case")
        XCTAssertNil(dana.cacheHitRatio)
        XCTAssertEqual(QuotaFormat.percent(dana.cacheHitRatio), QuotaFormat.notMeasured)
    }

    /// Round 2: `requests`/`inputTokens`/`outputTokens`/`cacheReadTokens`/
    /// `streamErrorCount` are now `null` on the wire whenever `source ==
    /// "offline"` (`src/cli.rs:1194-1263`) — `fetch_live_status` falls back to
    /// offline on `NoAnswer`/`Unusable` too, not just `NoServer`, so this is an
    /// ordinary state, not a rare one. Before these five fields became
    /// `Int?`, a synthesized `Decodable` threw `valueNotFound` on the first
    /// one it hit, and — because `Fleet.decode` decodes row-by-row —
    /// EVERY offline row failed identically, landing the whole fleet in
    /// `unreadable` instead of `accounts`. This is the test that would have
    /// caught it: it goes through the real `Fleet.decode` path, on an
    /// otherwise-valid row, exercising exactly the shape `tcr status --json`
    /// emits when offline.
    func testOfflineNullCountersDecodeTheRowInsteadOfFailingIt() throws {
        let json = """
            [
              {
                "source": "offline", "serverSha": null, "serverDirty": null,
                "name": "eli@example.com", "priority": 5, "status": "active",
                "disabled": false, "quota": null, "quotaState": "ok",
                "fiveHour": null, "sevenDay": null, "sevenDayOi": null, "held": [],
                "requests": null, "inputTokens": null, "outputTokens": null,
                "cacheReadTokens": null, "cacheHitRatio": null,
                "probeStatus": "skipped", "probeError": "no server",
                "lastStreamError": null, "streamErrorCount": null
              }
            ]
            """
        let decoded = try fleet(json)
        XCTAssertEqual(
            decoded.unreadableCount, 0,
            "the row must decode, not land in unreadable: \(decoded.unreadable)"
        )
        XCTAssertEqual(decoded.accounts.count, 1)
        let eli = decoded.accounts[0]
        XCTAssertNil(eli.requests)
        XCTAssertNil(eli.inputTokens)
        XCTAssertNil(eli.outputTokens)
        XCTAssertNil(eli.cacheReadTokens)
        XCTAssertNil(eli.streamErrorCount)
    }

    /// Round 3 fixed the compiler warning with `?? 0`, which silently turned
    /// an offline "not measured" row into a claimed zero-traffic reading —
    /// exactly the mistake ``QuotaFormat/percent(_:)``'s own doc comment
    /// names for the sibling `Double?` fields. This is the seam the
    /// coordinator asked for: `QuotaFormat.count(_:)` is the exact function
    /// both `FleetView.swift` call sites now route through, so asserting on
    /// its output here IS asserting on the rendered string, without a
    /// SwiftUI rendering harness.
    func testRenderedCountStringForPopulatedAndNullRows() throws {
        // Populated: alice (liveFixture) carries real, nonzero counters.
        let alice = try fleet(liveFixture).accounts[0]
        XCTAssertEqual(alice.requests, 128)
        XCTAssertEqual(QuotaFormat.count(alice.requests), "128")
        // bob (liveFixture) has streamErrorCount: 3, lastStreamError:
        // "overloaded_error" — the modifier-on-an-error case, which routes
        // through `streamErrorLabel(count:error:)` rather than `count(_:)`.
        let bob = try fleet(liveFixture).accounts[1]
        XCTAssertEqual(bob.streamErrorCount, 3)
        XCTAssertEqual(bob.lastStreamError, "overloaded_error")
        XCTAssertEqual(
            QuotaFormat.streamErrorLabel(count: bob.streamErrorCount, error: bob.lastStreamError!),
            "3× overloaded_error"
        )

        // Null: an offline row, same shape as the decode test above.
        let offlineJSON = """
            [
              {
                "source": "offline", "serverSha": null, "serverDirty": null,
                "name": "finn@example.com", "priority": 6, "status": "active",
                "disabled": false, "quota": null, "quotaState": "ok",
                "fiveHour": null, "sevenDay": null, "sevenDayOi": null, "held": [],
                "requests": null, "inputTokens": null, "outputTokens": null,
                "cacheReadTokens": null, "cacheHitRatio": null,
                "probeStatus": "skipped", "probeError": "no server",
                "lastStreamError": null, "streamErrorCount": null
              }
            ]
            """
        let finn = try fleet(offlineJSON).accounts[0]
        XCTAssertEqual(QuotaFormat.count(finn.requests), QuotaFormat.notMeasured)
        XCTAssertNotEqual(
            QuotaFormat.count(finn.requests), "0",
            "an offline account must never render as a measured zero-traffic row"
        )
        // The stream-error line makes a different, deliberate choice: a nil
        // count suppresses the multiplier rather than saying "n/a" — the
        // error string alone is still the actionable fact.
        XCTAssertEqual(
            QuotaFormat.streamErrorLabel(count: finn.streamErrorCount, error: "overloaded_error"),
            "overloaded_error"
        )
        XCTAssertFalse(
            QuotaFormat.streamErrorLabel(count: finn.streamErrorCount, error: "overloaded_error")
                .hasPrefix("n/a"),
            "a nil stream-error count must never render as the broken-English \"n/a×\""
        )
    }
}

/// ``Account/effectiveQuotaState(for:)`` is the pure selection
/// `FleetView`'s `fiveHourTint`/`sevenDayTint` key off — the per-window
/// field when present, else the shared composite `quotaState`. A fixture
/// where BOTH windows carry the same state (every other fixture in this
/// file, and every golden scene but `01c-divergent-windows`) cannot catch
/// a swapped `fiveHour`/`sevenDay` binding: reading the wrong field back
/// would still equal the right answer. These fixtures deliberately set
/// `fiveHourState` and `sevenDayState` to DIFFERENT values so a swap fails
/// loudly instead of rendering byte-identical to the correct code.
final class QuotaWindowStateTests: XCTestCase {
    private func account(fiveHourState: QuotaState, sevenDayState: QuotaState) -> Account {
        Account(
            name: "diverge@example.com",
            priority: 0,
            status: "active",
            disabled: false,
            quota: 0.5,
            quotaState: .near,
            fiveHour: 0.08,
            sevenDay: 0.96,
            sevenDayOi: 0.0,
            fiveHourState: fiveHourState,
            sevenDayState: sevenDayState,
            held: [],
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

    func testFiveHourWindowReadsItsOwnFieldNotSevenDays() {
        let a = account(fiveHourState: .ok, sevenDayState: .spent)
        XCTAssertEqual(a.effectiveQuotaState(for: .fiveHour), .ok)
        // The failure mode this guards: a binding that reads `sevenDayState`
        // for the 5h bar would return `.spent` here instead.
        XCTAssertNotEqual(a.effectiveQuotaState(for: .fiveHour), a.sevenDayState)
    }

    func testSevenDayWindowReadsItsOwnFieldNotFiveHours() {
        let a = account(fiveHourState: .ok, sevenDayState: .spent)
        XCTAssertEqual(a.effectiveQuotaState(for: .sevenDay), .spent)
        // The failure mode this guards: a binding that reads `fiveHourState`
        // for the 7d bar would return `.ok` here instead.
        XCTAssertNotEqual(a.effectiveQuotaState(for: .sevenDay), a.fiveHourState)
    }

    func testBothWindowsDivergeFromEachOtherInTheFixture() {
        // Belt-and-suspenders on the fixture itself: if a future edit ever
        // let these two collapse to the same value, the two tests above
        // would stop being able to catch a swap at all, silently.
        let a = account(fiveHourState: .ok, sevenDayState: .spent)
        XCTAssertNotEqual(a.fiveHourState, a.sevenDayState)
    }

    func testAbsentPerWindowFieldFallsBackToTheCompositeState() {
        // Older `tcr`: the per-window fields are absent (`nil`), not merely
        // unset in this fixture. Both windows must fall back to the shared
        // `quotaState` — the pre-existing single-bar behaviour — rather than
        // some other default.
        let a = Account(
            name: "legacy@example.com",
            priority: 0,
            status: "active",
            disabled: false,
            quota: 0.5,
            quotaState: .near,
            fiveHour: 0.5,
            sevenDay: 0.5,
            sevenDayOi: 0.0,
            held: [],
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
        XCTAssertEqual(a.effectiveQuotaState(for: .fiveHour), .near)
        XCTAssertEqual(a.effectiveQuotaState(for: .sevenDay), .near)
    }
}

/// ``Account/quotaBarTintSource(for:)`` — the fix for a real bug found on
/// pre-merge review: an account whose 7-day window is genuinely spent and
/// whose 5-hour window has never reported must show a NEUTRAL 5h bar, not
/// one inheriting the 7d window's red. `effectiveQuotaState(for:)` alone
/// cannot make this call — `fiveHourState == nil` cannot distinguish "no
/// reading" from "old server, field absent" — so `quotaBarTintSource` gates
/// on the FRACTION (`fiveHour`/`sevenDay`, populated on old and new wire
/// alike) instead. Golden-scene proof: `01d-unmeasured-window-proof`.
final class QuotaBarTintSourceTests: XCTestCase {
    private func account(
        quotaState: QuotaState,
        fiveHour: Double?,
        fiveHourState: QuotaState?,
        sevenDay: Double?,
        sevenDayState: QuotaState?
    ) -> Account {
        Account(
            name: "tint-source@example.com",
            priority: 0,
            status: "active",
            disabled: false,
            quota: sevenDay ?? fiveHour,
            quotaState: quotaState,
            fiveHour: fiveHour,
            sevenDay: sevenDay,
            sevenDayOi: 0.0,
            fiveHourState: fiveHourState,
            sevenDayState: sevenDayState,
            held: [],
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

    /// The exact bug: 7d genuinely spent, 5h has never reported a fraction
    /// at all. The 5h bar must be `.unmeasured`, never `.state(.spent)`
    /// borrowed from the composite/7d state.
    func testAWindowWithNoFractionIsUnmeasuredNotBorrowedFromItsSibling() {
        let a = account(
            quotaState: .spent,
            fiveHour: nil, fiveHourState: nil,
            sevenDay: 1.0, sevenDayState: .spent)
        XCTAssertEqual(a.quotaBarTintSource(for: .fiveHour), .unmeasured)
        // The failure mode this guards: reading `effectiveQuotaState`
        // directly (the pre-fix code path) would return `.state(.spent)`
        // here instead, since `fiveHourState ?? quotaState` falls through to
        // the composite `.spent`.
        XCTAssertNotEqual(a.quotaBarTintSource(for: .fiveHour), .state(.spent))
        // The sibling with a real reading still reports its own state.
        XCTAssertEqual(a.quotaBarTintSource(for: .sevenDay), .state(.spent))
    }

    /// The inverse shape, for symmetry: 5h has a real spent reading, 7d has
    /// never reported.
    func testTheOtherWindowWithNoFractionIsAlsoUnmeasured() {
        let a = account(
            quotaState: .spent,
            fiveHour: 1.0, fiveHourState: .spent,
            sevenDay: nil, sevenDayState: nil)
        XCTAssertEqual(a.quotaBarTintSource(for: .fiveHour), .state(.spent))
        XCTAssertEqual(a.quotaBarTintSource(for: .sevenDay), .unmeasured)
        XCTAssertNotEqual(a.quotaBarTintSource(for: .sevenDay), .state(.spent))
    }

    /// The genuine old-server shape this fallback still needs to serve: a
    /// fraction IS present (not nil), only the state WORD is missing —
    /// `quotaBarTintSource` must still borrow the composite state here,
    /// exactly as `effectiveQuotaState` always has.
    func testAPresentFractionWithNoStateWordStillBorrowsTheComposite() {
        let a = account(
            quotaState: .near,
            fiveHour: 0.5, fiveHourState: nil,
            sevenDay: 0.5, sevenDayState: nil)
        XCTAssertEqual(a.quotaBarTintSource(for: .fiveHour), .state(.near))
        XCTAssertEqual(a.quotaBarTintSource(for: .sevenDay), .state(.near))
    }
}

/// ``QuotaFormat`` carries the one honesty rule this whole task is about — a
/// nil measurement must never print or draw as a real zero — and until now
/// nothing exercised it directly; every existing test only asserted on the
/// upstream `Double?`, never on what the formatter turns it into.
final class QuotaFormatTests: XCTestCase {
    func testNilPercentIsNotMeasuredNeverZeroPercent() {
        XCTAssertEqual(QuotaFormat.percent(nil), "n/a")
        XCTAssertNotEqual(QuotaFormat.percent(nil), "0%")
    }

    func testRealZeroStillPrintsAsZeroPercent() {
        // The other half of the same contract: a genuine zero is a
        // measurement and must keep looking like one, or "0%" would stop
        // being trustworthy too.
        XCTAssertEqual(QuotaFormat.percent(0.0), "0%")
    }

    func testMeasuredPercentRounds() {
        XCTAssertEqual(QuotaFormat.percent(0.415), "42%")
        XCTAssertEqual(QuotaFormat.percent(1.0), "100%")
    }

    func testBarFillDistinguishesNilFromZero() {
        XCTAssertEqual(QuotaFormat.barFill(nil), .unmeasured)
        XCTAssertEqual(QuotaFormat.barFill(0.0), .measured(0.0))
        XCTAssertNotEqual(QuotaFormat.barFill(nil), QuotaFormat.barFill(0.0))
    }

    func testBarFillClampsOutOfRangeValues() {
        XCTAssertEqual(QuotaFormat.barFill(1.4), .measured(1.0))
        XCTAssertEqual(QuotaFormat.barFill(-0.2), .measured(0.0))
    }

    func testCountFormatsPresentValueVerbatim() {
        XCTAssertEqual(QuotaFormat.count(102), "102")
        XCTAssertEqual(QuotaFormat.count(0), "0")
    }

    func testNilCountIsNotMeasuredNeverZero() {
        XCTAssertEqual(QuotaFormat.count(nil), "n/a")
        XCTAssertNotEqual(QuotaFormat.count(nil), "0")
    }

    func testNilResetAtMsIsNeverAPlaceholderCaption() {
        XCTAssertNil(QuotaFormat.resetCaption(resetAtMs: nil, now: Date()))
    }

    func testPastResetYieldsNilNotANegativeCaption() {
        let now = Date()
        let past = now.addingTimeInterval(-60)
        XCTAssertNil(
            QuotaFormat.resetCaption(resetAtMs: Int64(past.timeIntervalSince1970 * 1000), now: now)
        )
    }

    func testResetCaptionMinutesTier() {
        let now = Date()
        let reset = now.addingTimeInterval(45 * 60)
        XCTAssertEqual(
            QuotaFormat.resetCaption(resetAtMs: Int64(reset.timeIntervalSince1970 * 1000), now: now),
            "resets in 45m"
        )
    }

    func testResetCaptionHoursAndMinutesTier() {
        let now = Date()
        let reset = now.addingTimeInterval((2 * 60 + 14) * 60)
        XCTAssertEqual(
            QuotaFormat.resetCaption(resetAtMs: Int64(reset.timeIntervalSince1970 * 1000), now: now),
            "resets in 2h 14m"
        )
    }

    func testResetCaptionDaysAndHoursTier() {
        let now = Date()
        let reset = now.addingTimeInterval((4 * 24 * 60 + 12 * 60) * 60)
        XCTAssertEqual(
            QuotaFormat.resetCaption(resetAtMs: Int64(reset.timeIntervalSince1970 * 1000), now: now),
            "resets in 4d 12h"
        )
    }
}

/// Hand-built accounts for the capacity aggregates. Only the fields the
/// aggregates read carry meaning; the rest are inert. Names stay obviously fake.
private func account(
    _ name: String,
    state: QuotaState,
    disabled: Bool = false,
    held: [HeldWindow] = [],
    groups: [String]? = nil
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
        serverDirty: false,
        groups: groups
    )
}

/// A dead-credential account: `status:"error"`, never probed — the live shape
/// (`src/manager/refresh.rs:93-101` rejects the refresh token and sets
/// `AccountStatus::Error`), not a hand-picked one. `quotaState` stays the
/// Rust-side default `ok` and `quota` stays `nil`, exactly as `hasQuotaEvidence`
/// expects for an account that has never been probed.
private func brokenAccount(_ name: String, disabled: Bool = false) -> Account {
    Account(
        name: name,
        priority: 1,
        status: "error",
        disabled: disabled,
        quota: nil,
        quotaState: .ok,
        fiveHour: nil,
        sevenDay: nil,
        sevenDayOi: nil,
        held: [],
        requests: 0,
        inputTokens: 0,
        outputTokens: 0,
        cacheReadTokens: 0,
        cacheHitRatio: nil,
        probeStatus: .never,
        probeError: nil,
        lastStreamError: nil,
        streamErrorCount: 0,
        source: .live,
        serverSha: "abc1234",
        serverDirty: false
    )
}

/// A credential that died AFTER being probed — the shape a review of the
/// original fix found every existing test blind to. `probe_account`
/// (`src/manager/probing.rs:128-139`) early-returns on an `Error` row instead
/// of clearing anything, and `refresh.rs:93-101` sets only `status` — so the
/// LAST-LEARNED `quota`, `quotaState` and `probeStatus: .ok` stay exactly as
/// they were the moment before the refresh token was rejected. `brokenAccount`
/// above (`probeStatus: .never, quota: nil`) cannot express this: it is the
/// OTHER way a broken account arrives, and the two are not interchangeable —
/// `hasQuotaEvidence` reads true here and false there.
private func probedThenBrokenAccount(
    _ name: String,
    quota: Double = 0.12,
    quotaState: QuotaState = .ok,
    disabled: Bool = false
) -> Account {
    Account(
        name: name,
        priority: 1,
        status: "error",
        disabled: disabled,
        quota: quota,
        quotaState: quotaState,
        fiveHour: quota,
        sevenDay: quota,
        sevenDayOi: 0,
        held: [],
        requests: 102,
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

/// The bug this whole change fixes: `status == "error"` paired with
/// `probeStatus == .never` — a rejected refresh token, not an absence of
/// probing — decoded and counted correctly.
final class AccountHealthTests: XCTestCase {
    func testErrorStatusIsNeedsRelogin() {
        XCTAssertEqual(brokenAccount("dave@example.com").health, .needsRelogin)
    }

    /// Nothing pins the `"error"` token across the Rust/Swift boundary except
    /// this test. `Account.health`'s `case "error": return .needsRelogin` is a
    /// bare string match against `AccountStatus::as_str` (`src/manager/mod.rs`)
    /// — a rename there degrades SILENTLY to `.other` here (by design, for a
    /// genuinely unrecognised future status), and every row would quietly
    /// revert to the pre-fix wrong pill with no red build to catch it.
    /// `testUnknownFutureStatusDecodesToOtherAndChangesNoCount` proves graceful
    /// degradation; this proves the token that degradation is silently hiding
    /// hasn't disappeared. Reads the Rust source directly rather than
    /// hardcoding a duplicate assumption about it, so a rename fails THIS
    /// test loudly instead of the panel failing silently in production.
    func testAccountStatusErrorTokenStillExistsInRustSource() throws {
        let thisFile = URL(fileURLWithPath: #filePath)
        let repoRoot =
            thisFile
            .deletingLastPathComponent()  // FleetStatusTests.swift -> TcrBarTests
            .deletingLastPathComponent()  // TcrBarTests -> Tests
            .deletingLastPathComponent()  // Tests -> apps/macos
            .deletingLastPathComponent()  // apps/macos -> apps
            .deletingLastPathComponent()  // apps -> repo root
        let rustSource = repoRoot.appendingPathComponent("src/manager/mod.rs")
        let contents = try String(contentsOf: rustSource, encoding: .utf8)
        XCTAssertTrue(
            contents.contains(#"AccountStatus::Error => "error","#),
            "the \"error\" token FleetStatus.swift's Account.health decodes as "
                + ".needsRelogin has moved or been renamed in \(rustSource.path) "
                + "— update the case in FleetStatus.swift to match, or every "
                + "broken-account row silently reverts to the pre-fix wrong pill"
        )
    }

    func testActiveAndThrottledDecodeToTheirOwnCases() {
        XCTAssertEqual(account("alice@example.com", state: .ok).health, .active)
        let throttled = Account(
            name: "bob@example.com", priority: 1, status: "throttled", disabled: false,
            quota: 0.9, quotaState: .near, fiveHour: 0.9, sevenDay: 0.9, sevenDayOi: 0,
            held: [], requests: 0, inputTokens: 0, outputTokens: 0, cacheReadTokens: 0,
            cacheHitRatio: nil, probeStatus: .ok, probeError: nil, lastStreamError: nil,
            streamErrorCount: 0, source: .live, serverSha: nil, serverDirty: nil
        )
        XCTAssertEqual(throttled.health, .throttled)
    }

    /// A future status this build has never seen degrades to `.other`, exactly
    /// like `QuotaState.unknown` and `ProbeState.unknown` do — never a decode
    /// failure, and it must not silently change any count.
    func testUnknownFutureStatusDecodesToOtherAndChangesNoCount() {
        let weird = Account(
            name: "eve@example.com", priority: 1, status: "quarantined", disabled: false,
            quota: nil, quotaState: .ok, fiveHour: nil, sevenDay: nil, sevenDayOi: nil,
            held: [], requests: 0, inputTokens: 0, outputTokens: 0, cacheReadTokens: 0,
            cacheHitRatio: nil, probeStatus: .never, probeError: nil, lastStreamError: nil,
            streamErrorCount: 0, source: .live, serverSha: nil, serverDirty: nil
        )
        XCTAssertEqual(weird.health, .other("quarantined"))
        let fleet = Fleet(accounts: [weird])
        // Not needs-relogin, so it falls through to the ordinary unmeasured
        // path — an unrecognised status must not invent a new remedy.
        XCTAssertEqual(fleet.needsReloginCount, 0)
        XCTAssertEqual(fleet.unmeasuredCount, 1)
    }

    /// `severity` used to fall through to `quotaState.severity` for a broken,
    /// enabled account — and a broken account's `quotaState` is the same Rust
    /// `#[default]` `"ok"` a never-probed one carries, so it scored `1`, tied
    /// with `disabled`, which this same property's doc-comment names as
    /// deliberately NOT an alarm. A dead credential is the opposite: worse
    /// than `spent`, not on par with an operator's own parked account.
    func testBrokenAccountOutranksSpentAndIsNotTiedWithDisabled() {
        let broken = brokenAccount("dave@example.com")
        let spent = account("erin@example.com", state: .spent)
        let disabled = account("off@example.com", state: .ok, disabled: true)

        XCTAssertGreaterThan(broken.severity, spent.severity)
        XCTAssertNotEqual(
            broken.severity, disabled.severity,
            "a dead credential is an alarm; a parked account is an operator's own choice"
        )
        XCTAssertEqual(Fleet(accounts: [spent, broken]).worst?.name, "dave@example.com")
    }
}

/// The invariant this whole change restores: a broken account is not
/// "unmeasured" — it is a known, certain fact with its own bucket, its own
/// count and a glyph that reflects the certainty rather than backing off to
/// `.unknown`.
final class FleetNeedsReloginTests: XCTestCase {
    func testBrokenAccountIsNotCountedAsUnmeasured() {
        let fleet = Fleet(accounts: [
            account("alice@example.com", state: .ok),
            brokenAccount("dave@example.com"),
        ])
        XCTAssertEqual(fleet.unmeasuredCount, 0, "an error account is not merely unprobed")
        XCTAssertEqual(fleet.needsReloginCount, 1)
    }

    func testBrokenAccountGetsItsOwnBreakdownBucket() {
        let fleet = Fleet(accounts: [
            account("alice@example.com", state: .ok),
            brokenAccount("dave@example.com"),
            brokenAccount("erin@example.com"),
        ])
        XCTAssertEqual(fleet.breakdownLabel, "1 ok · 2 need re-login")
    }

    func testCapacitySummaryNamesTheBrokenAccounts() {
        let fleet = Fleet(accounts: [
            account("alice@example.com", state: .ok),
            brokenAccount("dave@example.com"),
        ])
        XCTAssertEqual(fleet.capacitySummary, "1 of 2 ready · 1 need re-login")
    }

    /// The case that used to say "No confirmed capacity · 5 unmeasured" for a
    /// fleet where nothing was actually unmeasured — five accounts were dead
    /// credentials, a fact the fleet already knew and could have said.
    func testCapacitySummaryOnAllBrokenFleetDoesNotClaimUnconfirmed() {
        let fleet = Fleet(accounts: [
            brokenAccount("dave@example.com"),
            brokenAccount("erin@example.com"),
        ])
        XCTAssertEqual(fleet.capacitySummary, "No capacity · 2 need re-login")
        XCTAssertFalse(
            fleet.capacitySummary.contains("unmeasured"),
            "nothing here is unmeasured — every account has a known, rejected credential"
        )
    }

    /// The certainty a broken account earns: `.spent`, not `.unknown`. An
    /// unprobed account still forces `.unknown`, because that one really is a
    /// question mark — the two must not collapse into the same glyph.
    func testGlyphIsSpentNotUnknownWhenOnlyBrokenAccountsAreNotReady() {
        let brokenOnly = Fleet(accounts: [
            brokenAccount("dave@example.com"),
            brokenAccount("erin@example.com"),
        ])
        XCTAssertEqual(brokenOnly.capacityGlyphState, .spent)
        XCTAssertEqual(brokenOnly.capacityState, .spent)

        let unmeasuredOnly = Fleet(accounts: [
            Account(
                name: "frank@example.com", priority: 1, status: "active", disabled: false,
                quota: nil, quotaState: .ok, fiveHour: nil, sevenDay: nil, sevenDayOi: nil,
                held: [], requests: 0, inputTokens: 0, outputTokens: 0, cacheReadTokens: 0,
                cacheHitRatio: nil, probeStatus: .never, probeError: nil, lastStreamError: nil,
                streamErrorCount: 0, source: .live, serverSha: nil, serverDirty: nil
            )
        ])
        XCTAssertEqual(
            unmeasuredOnly.capacityGlyphState, .unknown("unmeasured"),
            "a genuinely never-probed account must still read as unknown, not spent"
        )
    }

    /// Row order: usable first, broken above the merely-unmeasured and the
    /// spent, parked last regardless of health.
    func testDisplayOrderPutsBrokenAboveUnmeasuredAndBelowReady() {
        let ready = account("alice@example.com", state: .ok)
        let broken = brokenAccount("dave@example.com")
        let neverProbed = Account(
            name: "frank@example.com", priority: 1, status: "active", disabled: false,
            quota: nil, quotaState: .ok, fiveHour: nil, sevenDay: nil, sevenDayOi: nil,
            held: [], requests: 0, inputTokens: 0, outputTokens: 0, cacheReadTokens: 0,
            cacheHitRatio: nil, probeStatus: .never, probeError: nil, lastStreamError: nil,
            streamErrorCount: 0, source: .live, serverSha: nil, serverDirty: nil
        )
        let spent = account("erin@example.com", state: .spent)
        let parkedBroken = brokenAccount("parked@example.com", disabled: true)

        let fleet = Fleet(accounts: [parkedBroken, spent, neverProbed, broken, ready])
        let order = fleet.rowsInDisplayOrder.map(\.name)
        XCTAssertEqual(
            order,
            [
                "alice@example.com", "dave@example.com", "frank@example.com",
                "erin@example.com", "parked@example.com",
            ]
        )
    }
}

/// `rowsInDisplayOrder(pinning:)`: the control account (normally `disabled`,
/// so it would otherwise sink to the very bottom via `displayOrder`) pinned
/// to the front, above even a fully-`ok` row — a stable partition, never a
/// re-sort of what remains.
///
/// The third edge case the feature's brief calls out — an older `tcr` with no
/// `control` subcommand at all — has no analog at this layer: this codebase
/// never decodes a `control` field off `Account` (see
/// `ControlAccountController`'s own doc-comment), so "the key is absent" is a
/// fact about `tcr control --show`, not about `tcr status --json`. That shape
/// is exercised in `ControlAccountCommandTests.testShowIsUnavailableOnANonZeroExit`
/// / `testControllerNeverReportsControlWhenUnavailable`, which prove
/// `ControlAccountController.current` stays `nil` in exactly that case — and a
/// `nil` name here is `testNoControlAccountLeavesOrderUnchanged` below.
final class FleetControlPinningTests: XCTestCase {
    func testControlAccountPinnedToFrontPreservesRelativeOrderOfTheRest() {
        // The control account is disabled — the common shape (parked out of
        // rotation) — and would otherwise sort dead last.
        let control = brokenAccount("zoe@example.com", disabled: true)
        let ready = account("alice@example.com", state: .ok)
        let near = account("bob@example.com", state: .near)
        let spent = account("carol@example.com", state: .spent)

        let fleet = Fleet(accounts: [spent, control, ready, near])
        // Without pinning: ready, near, spent, control (control sinks last).
        XCTAssertEqual(
            fleet.rowsInDisplayOrder.map(\.name),
            ["alice@example.com", "bob@example.com", "carol@example.com", "zoe@example.com"],
            "sanity check: unpinned, the control account is the worst-ranked row"
        )

        let pinned = fleet.rowsInDisplayOrder(pinning: "zoe@example.com").map(\.name)
        XCTAssertEqual(
            pinned,
            ["zoe@example.com", "alice@example.com", "bob@example.com", "carol@example.com"],
            "control row moves to the front; everyone else keeps their prior relative order"
        )
    }

    func testNoControlAccountLeavesOrderUnchanged() {
        let ready = account("alice@example.com", state: .ok)
        let spent = account("bob@example.com", state: .spent)
        let parked = account("carol@example.com", state: .ok, disabled: true)
        let fleet = Fleet(accounts: [spent, parked, ready])

        XCTAssertEqual(
            fleet.rowsInDisplayOrder(pinning: nil).map(\.name),
            fleet.rowsInDisplayOrder.map(\.name),
            "no control account set (the common case) must be byte-identical to today's order"
        )
    }

    func testControlNameNotPresentInTheFleetLeavesOrderUnchanged() {
        // Defensive: a stale/mismatched name (e.g. a fleet snapshot that
        // hasn't caught up with a just-cleared control account) must not
        // crash or silently drop a row — it must simply not pin anything.
        let ready = account("alice@example.com", state: .ok)
        let spent = account("bob@example.com", state: .spent)
        let fleet = Fleet(accounts: [spent, ready])

        XCTAssertEqual(
            fleet.rowsInDisplayOrder(pinning: "nobody@example.com").map(\.name),
            fleet.rowsInDisplayOrder.map(\.name)
        )
    }
}

/// The bug an adversarial review found: every test above exercises only
/// `brokenAccount` — never-probed, `quota: nil`. The shape that actually
/// happens in production is `probedThenBrokenAccount`: a credential that
/// dies AFTER being probed keeps its last-learned `quota`/`quotaState`, so
/// `hasQuotaEvidence` reads TRUE. `isReady` originally checked
/// `!disabled && quotaState == .ok && hasQuotaEvidence` with no health
/// clause, so this exact shape read READY — counted in `readyCount`, turned
/// `capacityState`/the glyph green, and the header could say
/// "1 of 1 ready · 1 need re-login" in the same frame.
final class FleetProbedThenBrokenTests: XCTestCase {
    func testProbedThenBrokenAccountIsNotReady() {
        let broken = probedThenBrokenAccount("dave@example.com")
        XCTAssertTrue(
            broken.hasQuotaEvidence,
            "the shape under test: this account DOES have a last-learned reading"
        )
        XCTAssertEqual(broken.quotaState, .ok, "and that reading says ok, same as a healthy account")
        XCTAssertFalse(
            broken.isReady,
            "hasQuotaEvidence + quotaState == .ok is not enough — health must gate isReady too"
        )
    }

    func testProbedThenBrokenAccountIsExcludedFromReadyCount() {
        let fleet = Fleet(accounts: [
            account("alice@example.com", state: .ok),
            probedThenBrokenAccount("dave@example.com"),
        ])
        XCTAssertEqual(fleet.readyCount, 1, "only alice; dave's stale ok reading must not count")
        XCTAssertEqual(fleet.needsReloginCount, 1)
        XCTAssertEqual(fleet.unmeasuredCount, 0, "dave has evidence — just not usable evidence")
    }

    func testCapacitySummaryDoesNotDoubleCountAProbedThenBrokenAccountAsReady() {
        let fleet = Fleet(accounts: [probedThenBrokenAccount("dave@example.com")])
        XCTAssertEqual(fleet.capacitySummary, "No capacity · 1 need re-login")
        XCTAssertFalse(
            fleet.capacitySummary.contains("of 1 ready"),
            "the exact contradiction the review found: \"1 of 1 ready\" beside \"1 need re-login\""
        )
    }

    func testCapacityStateIsSpentNotOkForAProbedThenBrokenAccount() {
        let fleet = Fleet(accounts: [probedThenBrokenAccount("dave@example.com")])
        XCTAssertEqual(fleet.capacityState, .spent)
        XCTAssertEqual(fleet.capacityGlyphState, .spent)
    }

    /// The `.near` glyph branch reads `quotaState` directly, not through
    /// `isReady` — a broken account whose LAST reading was `.near` (it was
    /// close to its threshold before the credential died) must not amber the
    /// glyph for capacity that has since gone to zero, not merely "close".
    func testCapacityGlyphIgnoresANearReadingFromABrokenAccount() {
        let fleet = Fleet(accounts: [
            probedThenBrokenAccount("dave@example.com", quota: 0.94, quotaState: .near)
        ])
        XCTAssertEqual(
            fleet.capacityGlyphState, .spent,
            "a broken account's stale .near reading must not amber the glyph"
        )
    }

    func testProbedThenBrokenAccountGetsTheNeedsReloginBreakdownBucketNotOk() {
        let fleet = Fleet(accounts: [
            account("alice@example.com", state: .ok),
            probedThenBrokenAccount("dave@example.com"),
        ])
        XCTAssertEqual(fleet.breakdownLabel, "1 ok · 1 need re-login")
    }

    /// Same class, different quota state: a spent-then-broken account must
    /// also land in `needsRelogin`, not `spent` — the bucket answers "why is
    /// this not ready", and the answer is the credential, not the quota.
    func testSpentThenBrokenAccountAlsoBucketsAsNeedsReloginNotSpent() {
        let fleet = Fleet(accounts: [
            probedThenBrokenAccount("dave@example.com", quota: 1.0, quotaState: .spent)
        ])
        XCTAssertEqual(fleet.breakdownLabel, "1 need re-login")
    }
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
        let fleet = Fleet(
            accounts: [
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
        let fleet = Fleet(
            accounts: (1...3).map {
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
        let fleet = Fleet(
            accounts: (1...3).map {
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
        // `query` resolves by exact name, then exact email (`src/identity.rs`,
        // `match_accounts`) — an abbreviated name matches nothing at all. Pass it
        // whole; the row already knows the exact value.
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

    func testExitZeroWithASilentStderrIsTheOnlyCleanSuccess() {
        XCTAssertEqual(AccountCommand.classify(enabling: true, exitCode: 0, stderr: ""), .clean)
        // Whitespace is not output.
        XCTAssertEqual(AccountCommand.classify(enabling: true, exitCode: 0, stderr: " \n"), .clean)
    }

    /// This test used to assert the opposite — "exit code decides, not stderr
    /// chatter" — and that sentence was the bug. `tcr` reports a park it could not
    /// persist by exiting 0 and warning on stderr (`src/cli.rs`), so exit 0 with
    /// output means *accepted, with something still to do*, and dropping the text
    /// here stamped `parked ✓` on a change that would not survive a restart.
    ///
    /// The rule is structural: ANY output, no phrase matched. A keyword list would
    /// pass every warning added to `tcr` after the day it was written.
    func testExitZeroWithAnythingOnStderrIsNotClean() {
        XCTAssertEqual(
            AccountCommand.classify(enabling: false, exitCode: 0, stderr: "some chatter"),
            .spoke(notice: "some chatter")
        )
        let notSaved =
            "[tcr] warning: NOT SAVED: no config entry matches this account "
            + "— it returns to rotation on restart"
        XCTAssertEqual(
            AccountCommand.classify(enabling: false, exitCode: 0, stderr: "\(notSaved)\n"),
            .spoke(notice: notSaved),
            "trimmed at the ends, otherwise verbatim — this app does not paraphrase tcr"
        )
    }

    func testAnAmbiguousQueryIsSurfacedVerbatim() {
        // The realistic failure: two accounts share an email across two orgs, so
        // the email fallback resolves to both and `tcr` refuses rather than picking.
        guard
            case .failed(let failure) = AccountCommand.classify(
                enabling: false,
                exitCode: 1,
                stderr: "  no unique account matches \"alice@example.com\" (2 candidates)\n"
            )
        else { return XCTFail("a non-zero exit must classify as failed") }
        XCTAssertEqual(failure.exitCode, 1)
        XCTAssertEqual(failure.message, "no unique account matches \"alice@example.com\" (2 candidates)")
        XCTAssertEqual(
            failure.summary,
            "disable failed (exit 1): no unique account matches \"alice@example.com\" (2 candidates)"
        )
    }

    func testASilentFailureStillSaysSomething() {
        guard case .failed(let failure) = AccountCommand.classify(enabling: true, exitCode: 2, stderr: "")
        else { return XCTFail("a non-zero exit must classify as failed") }
        XCTAssertEqual(failure.summary, "enable failed (exit 2): no output")
        XCTAssertFalse(failure.summary.isEmpty)
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

    func testTakeoverAsksForTheReplacementExplicitly() {
        // The takeover is expressed by the *presence* of `--replace`: tcr's own
        // singleton then does the replacing. Standing down is tcr's default now,
        // so an argument set that merely drops `--no-replace` takes over nothing —
        // which is why asserting the absence of `--no-replace` cannot stand alone.
        // That assertion stayed green through the entire window in which this
        // button was a silent no-op.
        XCTAssertTrue(
            ServerController.takeoverArguments.contains("--replace"),
            "\(ServerController.takeoverArguments) asks tcr for nothing — the takeover button is a no-op"
        )
        // Still checked, for a different reason than before: the two flags are a
        // hard clap conflict (`src/main.rs:198`), so `--no-replace` sneaking in
        // here would not cancel the `--replace` above — it would make the spawn
        // fail outright with a usage error and exit 2.
        XCTAssertFalse(
            ServerController.takeoverArguments.contains("--no-replace"),
            "\(ServerController.takeoverArguments) is a clap conflict — the spawn exits 2"
        )
        // And the safe set must never acquire the flag it exists to withhold.
        XCTAssertFalse(
            ServerController.safeArguments.contains("--replace"),
            "the routine start would kill a healthy incumbent: \(ServerController.safeArguments)"
        )
    }

    /// The regression that made every spawn path dead on arrival.
    ///
    /// Without `--headless`, `tcr server` runs its ratatui TUI
    /// (`src/main.rs:615`) which calls `enable_raw_mode()?` on stdout
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
        let known: Set<String> = ["server", "--no-replace", "--replace", "--headless"]
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
        let stderr =
            "[tcr] another proxy holds :3456 (pid 123) and --no-replace was set; "
            + "two proxies refreshing the same single-use tokens will token-war. Not replacing."
        let state = ServerController.classifyExit(exitCode: 1, stderr: stderr)
        guard case .incumbentHoldsPort = state else {
            return XCTFail("an already-running server must not be reported as an error")
        }
        XCTAssertFalse(state.isOurChild, "an incumbent we did not spawn is never ours to signal")
    }

    /// The wording above is what an *older* `tcr` printed. A current one stands
    /// down by default and says so differently — and, unlike the refusal, it exits
    /// **0**, because standing down is now the success path. So the classifier
    /// cannot lean on a non-zero exit code: without a marker match this would
    /// render "the server exited cleanly" for a server that never bound.
    func testTheStandDownIsRecognisedEvenThoughItExitsZero() {
        let stderr =
            "[tcr] another proxy holds :3456 (pid 123) and it is still listening — "
            + "leaving it alone and exiting without binding. Replacing it would wipe its "
            + "session→account pin map and cold-start every live session's prompt cache, the "
            + "most expensive event in this system. Pass --replace to take the port over anyway."
        guard
            case .incumbentHoldsPort = ServerController.classifyExit(
                intent: .safeStart, exitCode: 0, stderr: stderr
            )
        else {
            return XCTFail("a stand-down must read as an incumbent, not as a clean exit")
        }
        // Same text on the takeover path is the opposite verdict: the user asked
        // for the port and did not get it.
        guard
            case .takeoverRefused = ServerController.classifyExit(
                intent: .takeover, exitCode: 0, stderr: stderr
            )
        else {
            return XCTFail("a takeover that stood down has failed, however it exited")
        }
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
            defaults: UserDefaults(suiteName: "io.github.dhkts1.tcrbar.tests")!,
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
        let defaults = UserDefaults(suiteName: "io.github.dhkts1.tcrbar.tests")!
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
