import XCTest

@testable import TcrBarCore

/// The Fable weekly window on the card's 7d line.
///
/// It is a THIRD quota window with its own reset, gating Fable requests only
/// (`docs/cli.md`, "The weekly quota pair on `--json`"): a non-Fable request
/// never checks it, and `held[]`/`quotaState` never reflect it. So it cannot be
/// read off the `7d` figure beside it, and until this slot existed the panel
/// drew no Fable figure at all while the router was already gating on one.
///
/// One rule throughout, the same one the spend figures follow: a figure the
/// server did not measure is ABSENT — not `n/a`, not `0%` — and every figure
/// that is drawn says which window and which unit it is.
///
/// Fixtures use obviously-fake account names only — see CLAUDE.md.
final class FableWindowTests: XCTestCase {

    /// A fixed instant, so a caption's digits do not depend on when the suite
    /// runs. `resetCaption` refuses a reset that is not in the future, which is
    /// its own case below.
    private let now = Date(timeIntervalSince1970: 1_767_225_600)  // 2026-01-01T00:00:00Z
    private var fourDaysTwelveHours: Int64 { 1_767_225_600_000 + (4 * 1440 + 720) * 60_000 }

    // MARK: The wire

    func testTheWindowsOwnStateAndResetDecode() throws {
        let alice = try decoded(oi: "0.71", state: "\"near\"", reset: "\(fourDaysTwelveHours)")
        XCTAssertEqual(alice.sevenDayOi, 0.71)
        XCTAssertEqual(alice.sevenDayOiState, .near)
        XCTAssertEqual(alice.sevenDayOiResetAtMs, fourDaysTwelveHours)
    }

    /// Forward compatibility, and the case a LIVE proxy is in right now:
    /// `sevenDayOi` has been on the wire far longer than the two keys beside
    /// it, so a fraction with no state and no reset is the ordinary shape, not
    /// an exotic one. The row must decode, not throw.
    func testAnOlderServerSendsTheFractionAloneAndTheRowStillDecodes() throws {
        let older = """
            [{"name":"alice@example.com","priority":0,"status":"active","disabled":false,
              "quota":0.71,"quotaState":"ok","fiveHour":0.1,"sevenDay":0.2,"sevenDayOi":0.71,
              "held":[],"requests":1,"inputTokens":1,"outputTokens":1,"cacheReadTokens":1,
              "cacheHitRatio":0.5,"probeStatus":"ok","probeError":null,"lastStreamError":null,
              "streamErrorCount":0,"source":"live","serverSha":"abc1234","serverDirty":false}]
            """
        let alice = try XCTUnwrap(try Fleet.decode(Data(older.utf8)).accounts.first)
        XCTAssertEqual(alice.sevenDayOi, 0.71)
        XCTAssertNil(alice.sevenDayOiState, "no state word was sent — none is claimed")
        XCTAssertNil(alice.sevenDayOiResetAtMs)
        XCTAssertEqual(
            alice.fableWeeklyLabel(now: now), "fable 71%",
            "the percentage is still a measurement and still prints; only the caption is absent")
    }

    // MARK: What the slot draws

    func testTheSlotNamesTheWindowThePercentAndTheReset() throws {
        let alice = try decoded(oi: "0.71", state: "\"near\"", reset: "\(fourDaysTwelveHours)")
        XCTAssertEqual(alice.fableWeeklyLabel(now: now), "fable 71% · in 4d 12h")
    }

    /// The word `fable` is the span marker, the way `" today"` and `" out"` are
    /// on the 5h line: a bare `71%` in this slot is read as the 7-day
    /// percentage beside it restated.
    func testThePercentNeverPrintsWithoutItsWindow() throws {
        let alice = try decoded(oi: "0.71", state: "\"near\"", reset: "null")
        let label = try XCTUnwrap(alice.fableWeeklyLabel(now: now))
        XCTAssertTrue(label.hasPrefix("fable "), label)
        XCTAssertEqual(label, "fable 71%", "no live reset means no caption, never a blank one")
    }

    /// The rule the whole slot exists to hold. An older server, or an account
    /// this window was never learned for, draws NOTHING — not `n/a`, which is
    /// what a reader would otherwise take for a measured absence of headroom.
    func testAnUnmeasuredWindowDrawsNothingAtAll() throws {
        let never = try decoded(oi: "null", state: "null", reset: "null")
        XCTAssertNil(never.sevenDayOi)
        XCTAssertNil(never.fableWeeklyLabel(now: now))
        XCTAssertNil(never.fableWeeklySpokenLabel(now: now))
    }

    /// A measured zero is a reading — `0%` used, all the headroom there is —
    /// and must not be confused with the absent case above.
    func testAMeasuredZeroPrints() throws {
        let idle = try decoded(oi: "0.0", state: "\"ok\"", reset: "\(fourDaysTwelveHours)")
        XCTAssertEqual(idle.fableWeeklyLabel(now: now), "fable 0% · in 4d 12h")
    }

    /// `resetCaption` refuses a reset that is not in the future — the server
    /// only sends future ones, but a wire value ages between poll and draw. The
    /// percentage survives it; only the countdown goes.
    func testAnElapsedResetDropsTheCaptionAndKeepsTheFigure() throws {
        let stale = try decoded(
            oi: "0.71", state: "\"near\"", reset: "\(1_767_225_600_000 - 60_000)")
        XCTAssertEqual(stale.fableWeeklyLabel(now: now), "fable 71%")
    }

    // MARK: What VoiceOver hears

    func testTheSpokenFormNamesTheWindowInWordsAndSaysUsed() throws {
        let alice = try decoded(oi: "0.71", state: "\"near\"", reset: "\(fourDaysTwelveHours)")
        XCTAssertEqual(
            alice.fableWeeklySpokenLabel(now: now),
            "Fable weekly window: 71% used, near, resets in 4d 12h")
    }

    /// Utilization and headroom are the same number read two opposite ways, and
    /// a spoken percentage has no bar beside it to disambiguate.
    func testTheSpokenFormSaysUsedEvenWithNoStateWord() throws {
        let alice = try decoded(oi: "0.71", state: "null", reset: "null")
        XCTAssertEqual(
            alice.fableWeeklySpokenLabel(now: now), "Fable weekly window: 71% used",
            "no state word was measured, so none is spoken — never a fabricated ok")
    }

    // MARK: The counters that moved to a tooltip

    /// They used to print on this line, where the Fable figure now sits. Both
    /// figures say their span in the tooltip, which the one-line form could not
    /// afford: `102 req` was a count with no window attached, and a reader who
    /// took it for "today" was wrong by however long the proxy had been up.
    func testTheCountersTooltipSaysTheirSpan() throws {
        let alice = try decoded(oi: "0.71", state: "\"near\"", reset: "null")
        XCTAssertEqual(
            alice.countersTooltip(countersAreStructural: false),
            "102 requests served since this proxy started · 84% of their input tokens "
                + "came from cache")
    }

    /// On an offline read the counters are structurally zero because nothing
    /// measured them, so there is no tooltip at all — the same rule the printed
    /// line followed, not a zero dressed as a reading.
    func testStructuralCountersGetNoTooltip() throws {
        let alice = try decoded(oi: "0.71", state: "\"near\"", reset: "null")
        XCTAssertNil(alice.countersTooltip(countersAreStructural: true))
    }

    /// A `null` counter on the wire is `n/a`, never `0` — the same honesty rule
    /// every formatter in `QuotaFormat` states.
    func testANullCounterReadsAsNotMeasured() throws {
        let alice = try XCTUnwrap(
            try Fleet.decode(
                Data(
                    json(
                        oi: "0.71", state: "null", reset: "null",
                        requests: "null", cacheHitRatio: "null"
                    ).utf8)
            ).accounts.first)
        XCTAssertEqual(
            alice.countersTooltip(countersAreStructural: false),
            "n/a requests served since this proxy started · n/a of their input tokens "
                + "came from cache")
    }

    // MARK: Builders

    private func decoded(oi: String, state: String, reset: String) throws -> Account {
        try XCTUnwrap(
            try Fleet.decode(Data(json(oi: oi, state: state, reset: reset).utf8)).accounts.first)
    }

    private func json(
        oi: String,
        state: String,
        reset: String,
        requests: String = "102",
        cacheHitRatio: String = "0.84"
    ) -> String {
        """
        [{"name":"alice@example.com","priority":0,"status":"active","disabled":false,
          "quota":0.71,"quotaState":"ok","fiveHour":0.1,"fiveHourState":"ok",
          "sevenDay":0.2,"sevenDayState":"ok",
          "sevenDayOi":\(oi),"sevenDayOiState":\(state),"sevenDayOiResetAtMs":\(reset),
          "held":[],"requests":\(requests),"inputTokens":8781926,"outputTokens":31860,
          "cacheReadTokens":7407414,"cacheHitRatio":\(cacheHitRatio),"probeStatus":"ok",
          "probeError":null,"lastStreamError":null,"streamErrorCount":0,
          "source":"live","serverSha":"abc1234","serverDirty":false}]
        """
    }
}
