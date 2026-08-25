import XCTest

@testable import TcrBarCore

/// The `usage` object: decoding it, summing it, and rendering it.
///
/// One rule is under test throughout, in three shapes the wire produces
/// separately: `usage: null` (this account was not measured), `costUsd: null`
/// (this traffic could not be priced) and `window: null` (the server cannot
/// name when this quota window started). Each means something different, and
/// none of them is a zero — so the fleet's total must SKIP an unmeasured row
/// rather than add zero to itself, and the panel must draw nothing rather than
/// `$0.00`.
///
/// Fixtures use obviously-fake account names only — see CLAUDE.md.
final class UsageStatsTests: XCTestCase {

    // MARK: Fixtures

    /// A priced row with its own quota window, an unpriced row with no window,
    /// and a row from a server that never heard of `usage`.
    private let mixedFixture = """
        [
          {
            "source": "live", "serverSha": "abc1234", "serverDirty": false,
            "name": "alice@example.com", "priority": 0, "status": "active",
            "disabled": false, "quota": 0.12, "quotaState": "ok",
            "fiveHour": 0.12, "sevenDay": 0.04, "sevenDayOi": 0.0, "held": [],
            "requests": 100, "inputTokens": 8000000, "outputTokens": 30000,
            "cacheReadTokens": 6000000, "cacheCreationTokens": 1600000,
            "cacheHitRatio": 0.75, "probeStatus": "ok", "probeError": null,
            "lastStreamError": null, "streamErrorCount": 0,
            "usage": {
              "today": {
                "requests": 100, "inputTokens": 400000,
                "cacheCreationTokens": 1200000, "cacheCreation1hTokens": 400000,
                "cacheReadTokens": 6000000, "outputTokens": 30000,
                "costUsd": 14.0, "unpricedRequests": 0
              },
              "window": {
                "requests": 40, "inputTokens": 157000,
                "cacheCreationTokens": 471000, "cacheCreation1hTokens": 157000,
                "cacheReadTokens": 2355000, "outputTokens": 48000,
                "costUsd": 4.2, "unpricedRequests": 0, "since": 1767207600000
              },
              "lastHour": {
                "requests": 12, "inputTokens": 47000,
                "cacheCreationTokens": 141000, "cacheCreation1hTokens": 47000,
                "cacheReadTokens": 705000, "outputTokens": 3756,
                "costUsd": 3.0, "unpricedRequests": 0
              },
              "todayByModel": {
                "claude-opus-5": {
                  "requests": 70, "inputTokens": 280000,
                  "cacheCreationTokens": 840000, "cacheCreation1hTokens": 280000,
                  "cacheReadTokens": 4200000, "outputTokens": 21140,
                  "costUsd": 12.0, "unpricedRequests": 0
                },
                "claude-sonnet-5": {
                  "requests": 30, "inputTokens": 120000,
                  "cacheCreationTokens": 360000, "cacheCreation1hTokens": 120000,
                  "cacheReadTokens": 1800000, "outputTokens": 8860,
                  "costUsd": 2.0, "unpricedRequests": 0
                }
              }
            }
          },
          {
            "source": "live", "serverSha": "abc1234", "serverDirty": false,
            "name": "bob@example.com", "priority": 1, "status": "active",
            "disabled": false, "quota": 0.5, "quotaState": "ok",
            "fiveHour": 0.5, "sevenDay": 0.2, "sevenDayOi": 0.0, "held": [],
            "requests": 32, "inputTokens": 1000, "outputTokens": 900,
            "cacheReadTokens": 0, "cacheCreationTokens": 0,
            "cacheHitRatio": null, "probeStatus": "ok", "probeError": null,
            "lastStreamError": null, "streamErrorCount": 0,
            "usage": {
              "today": {
                "requests": 32, "inputTokens": 1000,
                "cacheCreationTokens": 0, "cacheCreation1hTokens": 0,
                "cacheReadTokens": 0, "outputTokens": 900,
                "costUsd": null, "unpricedRequests": 32
              },
              "window": null,
              "lastHour": {
                "requests": 4, "inputTokens": 100,
                "cacheCreationTokens": 0, "cacheCreation1hTokens": 0,
                "cacheReadTokens": 0, "outputTokens": 90,
                "costUsd": null, "unpricedRequests": 4
              },
              "todayByModel": {
                "claude-sonnet-4-5-20250929": {
                  "requests": 32, "inputTokens": 1000,
                  "cacheCreationTokens": 0, "cacheCreation1hTokens": 0,
                  "cacheReadTokens": 0, "outputTokens": 900,
                  "costUsd": null, "unpricedRequests": 32
                }
              }
            }
          },
          {
            "source": "live", "serverSha": "abc1234", "serverDirty": false,
            "name": "carol@example.com", "priority": 2, "status": "active",
            "disabled": false, "quota": 0.2, "quotaState": "ok",
            "fiveHour": 0.2, "sevenDay": 0.1, "sevenDayOi": 0.0, "held": [],
            "requests": 5, "inputTokens": 10, "outputTokens": 10,
            "cacheReadTokens": 0, "cacheHitRatio": null, "probeStatus": "ok",
            "probeError": null, "lastStreamError": null, "streamErrorCount": 0
          }
        ]
        """

    private func mixedFleet() throws -> Fleet {
        let fleet = try Fleet.decode(Data(mixedFixture.utf8))
        XCTAssertTrue(fleet.unreadable.isEmpty, "unreadable: \(fleet.unreadable)")
        return fleet
    }

    private func account(named name: String, in fleet: Fleet) throws -> Account {
        try XCTUnwrap(fleet.accounts.first { $0.name == name })
    }

    // MARK: Decoding — the three nulls stay apart

    func testEveryUsageShapeDecodes() throws {
        let fleet = try mixedFleet()
        XCTAssertEqual(fleet.accounts.count, 3, "a null anywhere must not cost a row")

        let priced = try account(named: "alice@example.com", in: fleet)
        let priced5h = try XCTUnwrap(priced.usage?.window)
        XCTAssertEqual(priced5h.costUsd, 4.2)
        XCTAssertEqual(priced5h.outputTokens, 48_000)
        XCTAssertEqual(priced5h.since, 1_767_207_600_000)
        XCTAssertEqual(priced.usage?.today.costUsd, 14.0)
        XCTAssertEqual(priced.cacheCreationTokens, 1_600_000)

        let unpriced = try account(named: "bob@example.com", in: fleet)
        let unpricedUsage = try XCTUnwrap(unpriced.usage)
        XCTAssertNil(
            unpricedUsage.today.costUsd,
            "a model with no published rate is null, never 0.0")
        XCTAssertEqual(unpricedUsage.today.unpricedRequests, 32)
        XCTAssertNil(
            unpricedUsage.window,
            "an unknown window start is null — a different fact from an empty window")
        XCTAssertEqual(
            unpricedUsage.windowOrToday.outputTokens, 900,
            "with no window the card falls back to today, which IS measured")

        let older = try account(named: "carol@example.com", in: fleet)
        XCTAssertNil(older.usage, "a server that predates the field reports nothing, not zero")
        XCTAssertNil(older.cacheCreationTokens)
    }

    // MARK: Fleet sums

    func testFleetTotalSkipsTheUnmeasuredAndCountsWhatItMissed() throws {
        let fleet = try mixedFleet()
        XCTAssertTrue(fleet.hasUsage)
        XCTAssertEqual(
            try XCTUnwrap(fleet.todayCost), 14.0, accuracy: 0.0001,
            "the unpriced row contributes nothing — adding its null as 0 would be the same "
                + "number for the wrong reason, so the unpriced count below is the real guard")
        XCTAssertEqual(
            fleet.todayUnpricedRequests, 32,
            "the requests missing from that total are counted, so a partial total says so")
        XCTAssertEqual(try XCTUnwrap(fleet.lastHourCost), 3.0, accuracy: 0.0001)
    }

    func testAnUnpricedFleetReportsNoCostRatherThanZero() throws {
        let fleet = Fleet(accounts: [
            row(name: "alice@example.com", todayCost: nil, unpriced: 7),
            row(name: "bob@example.com", todayCost: nil, unpriced: 3),
        ])
        XCTAssertNil(fleet.todayCost, "nothing could be priced — that is not $0.00")
        XCTAssertEqual(fleet.todayUnpricedRequests, 10)
        XCTAssertEqual(QuotaFormat.usd(fleet.todayCost), "n/a")
    }

    func testAFleetWithNoMeasurementSaysNothingAtAll() throws {
        let fleet = Fleet(accounts: [row(name: "alice@example.com", usage: nil)])
        XCTAssertFalse(fleet.hasUsage)
        XCTAssertNil(fleet.todayCost)
        XCTAssertNil(fleet.lastHourCost)
        XCTAssertNil(
            fleet.todayUnpricedRequests,
            "no measurement is not a count of zero unpriced requests")
        XCTAssertNil(fleet.todayCacheHitRatio)
        XCTAssertNil(
            fleet.usageSummaryLine,
            "against an older server the header draws no line — not a line of zeros")
    }

    func testCacheHitRatioIsNilWithNothingToDivide() throws {
        let empty = totals(cost: 1.0)
        XCTAssertNil(
            empty.cacheHitRatio,
            "no input tokens means no ratio was measured, the same rule the wire's own "
                + "cacheHitRatio follows")
        let usage = UsageRow(today: empty, window: nil, lastHour: empty, todayByModel: [:])
        let fleet = Fleet(accounts: [row(name: "alice@example.com", usage: usage)])
        XCTAssertNil(fleet.todayCacheHitRatio)
    }

    func testFleetCacheHitRatioIsMeasuredAcrossRows() throws {
        let fleet = try mixedFleet()
        // reads 6_000_000 over (400_000 + 1_200_000 + 400_000 + 6_000_000) + bob's 1000.
        let ratio = try XCTUnwrap(fleet.todayCacheHitRatio)
        XCTAssertEqual(ratio, 6_000_000.0 / 8_001_000.0, accuracy: 0.0001)
    }

    func testModelSharesAreMergedAcrossAccountsAndRankedByCost() throws {
        let fleet = try mixedFleet()
        let share = fleet.todayModelShare
        XCTAssertEqual(share.map(\.model), ["opus-5", "sonnet-5"])
        XCTAssertEqual(share[0].share, 12.0 / 14.0, accuracy: 0.0001)
        XCTAssertEqual(
            fleet.todayByModel.keys.sorted(),
            ["claude-opus-5", "claude-sonnet-4-5-20250929", "claude-sonnet-5"],
            "the unpriced model is still in the map — it just has no cost to rank on")
    }

    func testModelShareFallsBackToOutputTokensWhenNothingIsPriced() throws {
        let fleet = Fleet(accounts: [
            row(
                name: "alice@example.com",
                usage: UsageRow(
                    today: totals(cost: nil, output: 300, unpriced: 3),
                    window: nil,
                    lastHour: totals(cost: nil, output: 300, unpriced: 3),
                    todayByModel: [
                        "claude-opus-5": totals(cost: nil, output: 300, unpriced: 3),
                        "claude-haiku-5": totals(cost: nil, output: 100, unpriced: 1),
                    ]))
        ])
        let share = fleet.todayModelShare
        XCTAssertEqual(share.map(\.model), ["opus-5", "haiku-5"])
        XCTAssertEqual(share[0].share, 0.75, accuracy: 0.0001)
    }

    // MARK: The header line

    func testHeaderLineNamesTwoModelsAndSaysWhatIsMissing() throws {
        let fleet = try mixedFleet()
        let line = try XCTUnwrap(fleet.usageSummaryLine)
        XCTAssertEqual(line, "$14.0 today · $3.00/hr · opus-5 86% · sonnet-5 14% · cache 75% · 32 unpriced")
    }

    func testHeaderLineCollapsesTheThirdModel() throws {
        let byModel = [
            "claude-opus-5": totals(cost: 6.0),
            "claude-sonnet-5": totals(cost: 3.0),
            "claude-haiku-5": totals(cost: 1.0),
        ]
        let fleet = Fleet(accounts: [
            row(
                name: "alice@example.com",
                usage: UsageRow(
                    today: totals(cost: 10.0, input: 100),
                    window: nil,
                    lastHour: totals(cost: 1.0),
                    todayByModel: byModel))
        ])
        let line = try XCTUnwrap(fleet.usageSummaryLine)
        XCTAssertTrue(line.contains("opus-5 60% · sonnet-5 30% · +1"), line)
        XCTAssertFalse(line.contains("unpriced"), "nothing was missing from this total: \(line)")
    }

    // MARK: The card's 5h figures

    func testCardShowsTheWindowAndFallsBackToTheDay() throws {
        let fleet = try mixedFleet()
        XCTAssertEqual(
            try account(named: "alice@example.com", in: fleet).windowUsageLabel, "$4.20 · 48k")
        XCTAssertEqual(
            try account(named: "bob@example.com", in: fleet).windowUsageLabel, "900",
            "no price means the token count prints alone, never beside a fabricated $0.00")
        XCTAssertNil(
            try account(named: "carol@example.com", in: fleet).windowUsageLabel,
            "an unmeasured account gets an empty slot")
        XCTAssertNil(try account(named: "carol@example.com", in: fleet).windowUsageSpokenLabel)
        XCTAssertEqual(
            try account(named: "alice@example.com", in: fleet).windowUsageSpokenLabel,
            "this window: $4.20, 48k output tokens")
    }

    // MARK: Formatters

    func testUsdKeepsTwoUsefulFiguresAndNeverInventsOne() {
        XCTAssertEqual(QuotaFormat.usd(0.42), "$0.42")
        XCTAssertEqual(QuotaFormat.usd(12.4157), "$12.4")
        XCTAssertEqual(QuotaFormat.usd(120.4), "$120")
        XCTAssertEqual(QuotaFormat.usd(0), "$0.00", "a measured zero is a reading")
        XCTAssertEqual(QuotaFormat.usd(nil), "n/a", "an unpriced bucket is not a free one")
    }

    func testTokensAreScaledAndNeverFabricated() {
        XCTAssertEqual(QuotaFormat.tokens(812), "812")
        XCTAssertEqual(QuotaFormat.tokens(48_000), "48k")
        XCTAssertEqual(QuotaFormat.tokens(1_240_000), "1.2M")
        XCTAssertEqual(QuotaFormat.tokens(1_200), "1.2k")
        XCTAssertEqual(QuotaFormat.tokens(0), "0")
        XCTAssertEqual(QuotaFormat.tokens(nil), "n/a")
    }

    func testModelLabelDropsOnlyWhatEveryModelShares() {
        XCTAssertEqual(QuotaFormat.modelLabel("claude-opus-5"), "opus-5")
        XCTAssertEqual(QuotaFormat.modelLabel("claude-sonnet-4-5-20250929"), "sonnet-4-5")
        XCTAssertEqual(
            QuotaFormat.modelLabel("some-other-model"), "some-other-model",
            "an id this rule does not recognise prints verbatim rather than guessed at")
        XCTAssertEqual(
            QuotaFormat.modelLabel("claude-opus-5-2025"), "opus-5-2025",
            "four digits is not a release date; only an eight-digit tail is dropped")
    }

    // MARK: Builders

    private func totals(
        cost: Double?,
        input: Int = 0,
        output: Int = 0,
        cacheRead: Int = 0,
        unpriced: Int = 0
    ) -> UsageTotals {
        UsageTotals(
            requests: 1,
            inputTokens: input,
            cacheCreationTokens: 0,
            cacheCreation1hTokens: 0,
            cacheReadTokens: cacheRead,
            outputTokens: output,
            costUsd: cost,
            unpricedRequests: unpriced)
    }

    private func row(name: String, todayCost: Double?, unpriced: Int) -> Account {
        row(
            name: name,
            usage: UsageRow(
                today: totals(cost: todayCost, input: 10, unpriced: unpriced),
                window: nil,
                lastHour: totals(cost: todayCost, unpriced: unpriced),
                todayByModel: [:]))
    }

    private func row(name: String, usage: UsageRow?) -> Account {
        Account(
            name: name,
            priority: 0,
            status: "active",
            disabled: false,
            quota: 0.1,
            quotaState: .ok,
            fiveHour: 0.1,
            sevenDay: 0.1,
            sevenDayOi: 0.0,
            held: [],
            requests: 1,
            inputTokens: 1,
            outputTokens: 1,
            cacheReadTokens: 1,
            cacheHitRatio: nil,
            probeStatus: .ok,
            probeError: nil,
            lastStreamError: nil,
            streamErrorCount: 0,
            source: .live,
            serverSha: "abc1234",
            serverDirty: false,
            usage: usage)
    }
}
