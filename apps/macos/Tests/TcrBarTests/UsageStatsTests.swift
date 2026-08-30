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
                "cacheCreationTokens": 1600000, "cacheCreation1hTokens": 400000,
                "cacheReadTokens": 6000000, "outputTokens": 30000,
                "costUsd": 14.0, "unpricedRequests": 0
              },
              "window": {
                "requests": 40, "inputTokens": 157000,
                "cacheCreationTokens": 628000, "cacheCreation1hTokens": 157000,
                "cacheReadTokens": 2355000, "outputTokens": 48000,
                "costUsd": 4.2, "unpricedRequests": 0, "since": 1767207600000
              },
              "lastHour": {
                "requests": 12, "inputTokens": 47000,
                "cacheCreationTokens": 188000, "cacheCreation1hTokens": 47000,
                "cacheReadTokens": 705000, "outputTokens": 3756,
                "costUsd": 3.0, "unpricedRequests": 0
              },
              "todayByModel": {
                "claude-opus-5": {
                  "requests": 70, "inputTokens": 280000,
                  "cacheCreationTokens": 1120000, "cacheCreation1hTokens": 280000,
                  "cacheReadTokens": 4200000, "outputTokens": 21140,
                  "costUsd": 12.0, "unpricedRequests": 0
                },
                "claude-sonnet-5": {
                  "requests": 30, "inputTokens": 120000,
                  "cacheCreationTokens": 480000, "cacheCreation1hTokens": 120000,
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
        XCTAssertEqual(
            priced.usage?.today.cacheCreationTokens, 1_600_000,
            "ALL cache creation, both TTLs — the same quantity the row-level counter carries")
        XCTAssertEqual(
            priced.usage?.today.cacheCreation1hTokens, 400_000,
            "and the 1-hour part is a SUBSET of it, never an addend")

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
        // reads 6_000_000 over (400_000 + 1_600_000 + 6_000_000) + bob's 1000. The
        // 1-hour cache creation is INSIDE that 1_600_000 and is not added again.
        let ratio = try XCTUnwrap(fleet.todayCacheHitRatio)
        XCTAssertEqual(ratio, 6_000_000.0 / 8_001_000.0, accuracy: 0.0001)
    }

    func testModelSharesAreMergedAcrossAccountsAndRankedByCost() throws {
        let fleet = try mixedFleet()
        let share = fleet.todayModelShare
        XCTAssertEqual(
            share.map(\.model), ["opus-5", "sonnet-5", "sonnet-4-5"],
            "the unpriced model is listed too — after the priced ones, and with no percentage")
        XCTAssertEqual(try XCTUnwrap(share[0].share), 12.0 / 14.0, accuracy: 0.0001)
        XCTAssertNil(share[2].share, "nobody can compute a cost share for an unpriced model")
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
        XCTAssertEqual(try XCTUnwrap(share[0].share), 0.75, accuracy: 0.0001)
    }

    // MARK: The header line

    /// The unpriced model is COUNTED now. It used to be filtered out of the
    /// ranking entirely, so this line named two models and collapsed nothing,
    /// while a third model with 32 of the fleet's 132 requests appeared
    /// nowhere — the panel read as "all opus" about a fleet that was not.
    ///
    /// That third model is now NAMED, not counted: the `"+1"` this line used to
    /// end its model list with said only that something was missing, without
    /// saying what.
    func testHeaderLineNamesEveryModelAndSaysWhatIsMissing() throws {
        let fleet = try mixedFleet()
        let line = try XCTUnwrap(fleet.usageSummaryLine)
        XCTAssertEqual(
            line,
            "$14.0 today · $3.00/hr · opus-5 86% · sonnet-5 14% · sonnet-4-5 ? · cache 75% "
                + "· 32 unpriced today")
        XCTAssertFalse(line.contains("+1"), "nothing is collapsed any more: \(line)")
    }

    func testHeaderLineNamesTheThirdModelRatherThanCountingIt() throws {
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
        XCTAssertTrue(line.contains("opus-5 60% · sonnet-5 30% · haiku-5 10% · cache"), line)
        XCTAssertFalse(line.contains("unpriced"), "nothing was missing from this total: \(line)")
    }

    /// The case naming every model exposes, on the real fleet that motivated
    /// it: $5,079 of opus beside $1.55 of haiku is a 0.03% slice, and
    /// `Int((0.0003 * 100).rounded())` is `0`. `haiku-4-5 0%` states that a
    /// model which served today spent nothing — the same false zero
    /// ``QuotaFormat/percent(_:)`` refuses for a `nil`, arriving instead through
    /// the rounding. It was unreachable while only the top two models were
    /// named, because a slice that small is never in the top two.
    func testATinySliceSaysLessThanOnePercentRatherThanZero() throws {
        let fleet = Fleet(accounts: [
            row(
                name: "alice@example.com",
                usage: UsageRow(
                    today: totals(cost: 5081.20, input: 100, requests: 900),
                    window: nil,
                    lastHour: totals(cost: 1.0),
                    todayByModel: [
                        "claude-opus-5": totals(cost: 5079.65, requests: 800),
                        "claude-haiku-4-5-20251001": totals(cost: 1.55, requests: 100),
                    ]))
        ])
        let line = try XCTUnwrap(fleet.usageSummaryLine)
        XCTAssertTrue(line.contains("opus-5 100% · haiku-4-5 <1%"), line)
        XCTAssertFalse(
            line.contains("haiku-4-5 0%"),
            "this model served 100 requests and cost $1.55 — it did not spend nothing: \(line)")
    }

    /// The other side of that rule: a model that genuinely cost nothing prints
    /// the zero, because the zero is then the measurement. `<1%` for a real
    /// `0%` would be as wrong in the other direction.
    func testAMeasuredZeroShareStillPrintsZero() throws {
        XCTAssertEqual(QuotaFormat.share(0), "0%")
        XCTAssertEqual(QuotaFormat.share(nil), "n/a")
        XCTAssertEqual(QuotaFormat.share(0.0049), "<1%")
        XCTAssertEqual(QuotaFormat.share(0.005), "1%", "this one rounds up on its own")
        XCTAssertEqual(QuotaFormat.share(1), "100%")
    }

    // MARK: The card's 5h figures

    func testCardShowsTheWindowAndFallsBackToTheDay() throws {
        let fleet = try mixedFleet()
        XCTAssertEqual(
            try account(named: "alice@example.com", in: fleet).windowUsageLabel, "$4.20 · 48k out")
        XCTAssertEqual(
            try account(named: "bob@example.com", in: fleet).windowUsageLabel, "900 out today",
            "no price means the token count prints alone, never beside a fabricated $0.00 — "
                + "with its unit, and marked as the DAY, which is the bucket it came from")
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

    // MARK: Every figure says what span and what unit it is

    /// Finding 0. `window ?? today` puts up to 24 hours of spend in the slot
    /// beside a 5h percentage and a 5h countdown, and the card said nothing
    /// about which span it was — late in the day an operator reads a full
    /// day's spend as five hours' worth and concludes the account is burning
    /// three times faster than it is.
    func testADayFigureNeverWearsTheWindowsLabel() throws {
        let day = UsageRow(
            today: totals(cost: 9.4102, output: 32_000, requests: 102),
            window: nil,
            lastHour: totals(cost: 1.1021, requests: 12),
            todayByModel: [:])
        let account = row(name: "alice@example.com", usage: day)
        XCTAssertEqual(account.windowUsageLabel, "$9.41 today · 32k out")
        XCTAssertEqual(
            account.windowUsageSpokenLabel, "today: $9.41, 32k output tokens",
            "VoiceOver said \"this window\" for the same substituted figure")
        XCTAssertFalse(
            try XCTUnwrap(account.windowUsageSpokenLabel).contains("window"),
            "no string built from a day bucket may say window")
    }

    /// Finding 3. `Totals::to_wire` returns the PARTIAL sum whenever some of a
    /// bucket's requests could not be priced, and the card printed it as if it
    /// were the whole. The header's `N unpriced` clause cannot cover this: it
    /// is fleet-wide and computed from `today`, so an account whose WINDOW is
    /// partially unpriced in an otherwise-priced fleet got no warning at all.
    func testAPartiallyPricedWindowSaysItIsAFloor() throws {
        let partial = UsageRow(
            today: totals(cost: 12.84, output: 31_860, unpriced: 12, requests: 102),
            window: totals(cost: 5.6141, output: 12_476, unpriced: 12, requests: 40),
            lastHour: totals(cost: 1.6413, requests: 12),
            todayByModel: [:])
        let account = row(name: "alice@example.com", usage: partial)
        XCTAssertEqual(account.windowUsageLabel, "$5.61+ · 12k out")
        XCTAssertEqual(
            account.windowUsageSpokenLabel, "this window: at least $5.61, 12k output tokens")
    }

    /// Finding 7. The slot sits in one HStack beside a percentage and a
    /// countdown, so a bare `900` there reads as 900 requests, 900 dollars or a
    /// second percentage. Every form carries its unit.
    func testEveryFormOfTheCardFigureCarriesItsUnit() throws {
        func label(_ usage: UsageRow) throws -> String {
            try XCTUnwrap(row(name: "alice@example.com", usage: usage).windowUsageLabel)
        }
        let priced = try label(
            UsageRow(
                today: totals(cost: 14.0, requests: 102),
                window: totals(cost: 5.6141, output: 12_476, requests: 40),
                lastHour: totals(cost: 1.0), todayByModel: [:]))
        let unpriceable = try label(
            UsageRow(
                today: totals(cost: nil, unpriced: 102, requests: 102),
                window: totals(cost: nil, output: 12_476, unpriced: 40, requests: 40),
                lastHour: totals(cost: nil, unpriced: 12, requests: 12), todayByModel: [:]))
        let day = try label(
            UsageRow(
                today: totals(cost: 9.4102, output: 32_000, requests: 102),
                window: nil, lastHour: totals(cost: 1.1021), todayByModel: [:]))
        XCTAssertEqual(priced, "$5.61 · 12k out")
        XCTAssertEqual(unpriceable, "12k out", "a token count is not a bare number")
        XCTAssertEqual(day, "$9.41 today · 32k out")
        for form in [priced, unpriceable, day] {
            XCTAssertTrue(
                form.contains("out") || form.contains("$"),
                "a figure with no unit reads as whatever the number beside it is: \(form)")
        }
    }

    // MARK: A model with traffic is never dropped

    /// Finding 1. A model this build has no rate for used to be filtered out of
    /// the ranking entirely — not named, and not even counted by the `+N`
    /// clause that exists to say there are more models. The day a new model
    /// ships, that is most of the fleet's traffic.
    func testAnUnpricedModelIsNamedWithAQuestionMark() throws {
        let fleet = Fleet(accounts: [
            row(
                name: "alice@example.com",
                usage: UsageRow(
                    today: totals(cost: 12.0, input: 100, unpriced: 32, requests: 132),
                    window: nil,
                    lastHour: totals(cost: 1.0),
                    todayByModel: [
                        "claude-opus-5": totals(cost: 12.0, output: 21_140, requests: 100),
                        "claude-sonnet-4-5-20250929": totals(
                            cost: nil, output: 8_860, unpriced: 32, requests: 32),
                    ]))
        ])
        let share = fleet.todayModelShare
        XCTAssertEqual(share.map(\.model), ["opus-5", "sonnet-4-5"])
        XCTAssertNil(share[1].share)
        let line = try XCTUnwrap(fleet.usageSummaryLine)
        XCTAssertTrue(
            line.contains("opus-5 100% · sonnet-4-5 ?"),
            "the model with a third of the requests must be on the line: \(line)")
    }

    /// Finding 2. `modelLabel` strips `claude-` and an eight-digit release
    /// tail, so `claude-opus-5` and `claude-opus-5-20250929` are one model
    /// wearing two ids — the ordinary state during a rollout. Grouped on the
    /// raw id, the header drew the same name twice, halved its true share, and
    /// let `+N` count a model that does not exist.
    func testTwoIdsForOneModelCollapseToOneEntry() throws {
        let fleet = Fleet(accounts: [
            row(
                name: "alice@example.com",
                usage: UsageRow(
                    today: totals(cost: 10.0, input: 100, requests: 100),
                    window: nil,
                    lastHour: totals(cost: 1.0),
                    todayByModel: [
                        "claude-opus-5": totals(cost: 4.5, output: 1_000, requests: 45),
                        "claude-opus-5-20250929": totals(cost: 3.0, output: 700, requests: 30),
                        "claude-sonnet-5": totals(cost: 2.5, output: 500, requests: 25),
                    ]))
        ])
        let share = fleet.todayModelShare
        XCTAssertEqual(share.map(\.model), ["opus-5", "sonnet-5"])
        XCTAssertEqual(
            try XCTUnwrap(share[0].share), 0.75, accuracy: 0.0001,
            "both spellings are the same model, so its share is the sum of the two")
        let line = try XCTUnwrap(fleet.usageSummaryLine)
        XCTAssertTrue(
            line.contains("opus-5 75% · sonnet-5 25% · cache"),
            "two models ran, so the line names two — not three, one of them twice: \(line)")
        XCTAssertEqual(
            fleet.todayByModelLabel.keys.sorted(), ["opus-5", "sonnet-5"],
            "the merge happens on the label, before anything ranks or counts it")
    }

    /// Round two, finding 5. One label, two ids, one of them priceable.
    ///
    /// `todayByModelLabel` canonicalizes before grouping, so `claude-opus-5`
    /// and `claude-opus-5-20250929` merge — the ordinary state during a
    /// rollout. If this build has a rate for one id and not the other,
    /// `UsageTotals.adding` folds `(priced, nil)` into the priced value, and
    /// the merged bucket lands in `todayModelShare`'s PRICED head with a
    /// non-nil cost. The `?` the whole unpriced feature exists to show can
    /// never fire for it, so the header printed `opus-5 62%` — a clean
    /// percentage of a partial cost — and only the fleet-wide clause hinted
    /// otherwise. `partial` carries it out, and the figure wears the same `+`
    /// the card's own cost wears for the same reason.
    func testAMergedLabelSaysItsPercentIsAFloor() throws {
        let fleet = Fleet(accounts: [
            row(
                name: "alice@example.com",
                usage: UsageRow(
                    today: totals(cost: 20.0, input: 100, unpriced: 30, requests: 130),
                    window: nil,
                    lastHour: totals(cost: 1.0),
                    todayByModel: [
                        // One model, two ids. The dated one has no rate here.
                        "claude-opus-5": totals(cost: 12.0, output: 1_000, requests: 70),
                        "claude-opus-5-20250929": totals(
                            cost: nil, output: 400, unpriced: 30, requests: 30),
                        "claude-sonnet-5": totals(cost: 8.0, output: 600, requests: 30),
                    ]))
        ])
        let merged = try XCTUnwrap(fleet.todayByModelLabel["opus-5"])
        XCTAssertEqual(merged.costUsd, 12.0, "the priced half is all the cost there is")
        XCTAssertEqual(
            merged.unpricedRequests, 30,
            "and the unpriceable half is still counted, which is what makes the figure a floor")

        let share = fleet.todayModelShare
        XCTAssertEqual(share.map(\.model), ["opus-5", "sonnet-5"])
        XCTAssertTrue(share[0].partial, "30 of this label's requests are missing from its cost")
        XCTAssertFalse(share[1].partial, "sonnet-5 was priced in full")
        XCTAssertEqual(share[0].label, "opus-5 60%+")
        XCTAssertEqual(share[1].label, "sonnet-5 40%")

        let line = try XCTUnwrap(fleet.usageSummaryLine)
        XCTAssertTrue(line.contains("opus-5 60%+ · sonnet-5 40%"), line)
    }

    /// The `+` marks a partial cost, never an absent one. A label with no price
    /// at all keeps `?`, which already says everything a `+` would qualify.
    func testAnEntirelyUnpricedLabelKeepsItsQuestionMarkAndNoPlus() throws {
        let fleet = Fleet(accounts: [
            row(
                name: "alice@example.com",
                usage: UsageRow(
                    today: totals(cost: 12.0, input: 100, unpriced: 32, requests: 132),
                    window: nil,
                    lastHour: totals(cost: 1.0),
                    todayByModel: [
                        "claude-opus-5": totals(cost: 12.0, output: 21_140, requests: 100),
                        "claude-sonnet-4-5-20250929": totals(
                            cost: nil, output: 8_860, unpriced: 32, requests: 32),
                    ]))
        ])
        let share = fleet.todayModelShare
        XCTAssertEqual(share[1].label, "sonnet-4-5 ?")
        XCTAssertFalse(share[1].partial)
        XCTAssertEqual(share[0].label, "opus-5 100%", "this label was priced in full")
    }

    // MARK: A measured zero is a reading

    /// Finding 4. Between the server's local midnight and the first request of
    /// the new day, `today` is a bucket that served nothing. That is `$0.00` —
    /// `n/a` is the token this panel reserves for traffic nobody could price,
    /// and an operator opening the panel at 00:30 could not tell a quiet night
    /// from a pricing table that failed to load.
    func testAnIdleDayIsAMeasuredZeroNotAnAbsence() throws {
        let idle = totals(cost: nil, requests: 0)
        XCTAssertEqual(idle.measuredCost, 0, "nothing served is nothing spent")
        let fleet = Fleet(accounts: [
            row(
                name: "alice@example.com",
                usage: UsageRow(
                    today: idle, window: nil, lastHour: idle, todayByModel: [:]))
        ])
        XCTAssertEqual(QuotaFormat.usd(fleet.todayCost), "$0.00")
        XCTAssertEqual(
            try XCTUnwrap(fleet.usageSummaryLine), "$0.00 today · $0.00/hr · cache n/a",
            "an idle fleet reports what it spent; only the cache ratio is genuinely unmeasured")
        XCTAssertNil(
            totals(cost: nil, unpriced: 1, requests: 1).measuredCost,
            "a bucket that DID serve requests and could not price one is still an absence")
    }

    /// Finding 5. `n/a/hr` runs two tokens together and parses as a path, where
    /// every other `n/a` in this family stands alone as a word. The rate is
    /// dropped instead — and the HOUR says why, in its own clause. Round two's
    /// finding 4 is the reason that clause is not today's: the two buckets
    /// share no traffic across the server's local midnight, so today's count
    /// cannot speak for the hour, and ``testAnUnpriceableHourSaysSoEvenWhenTheDayIsEmpty``
    /// is the case that proves it.
    func testAnUnpriceableHourDropsTheRateRatherThanRunningItTogether() throws {
        let fleet = Fleet(accounts: [
            row(
                name: "alice@example.com",
                usage: UsageRow(
                    today: totals(cost: 14.0, input: 100, unpriced: 102, requests: 204),
                    window: nil,
                    lastHour: totals(cost: nil, unpriced: 12, requests: 12),
                    todayByModel: ["claude-opus-5": totals(cost: 14.0, requests: 102)]))
        ])
        let line = try XCTUnwrap(fleet.usageSummaryLine)
        XCTAssertFalse(line.contains("n/a/hr"), line)
        XCTAssertFalse(line.contains("/hr"), "an unpriceable hour has no rate to state: \(line)")
        XCTAssertTrue(line.hasPrefix("$14.0 today · 12 unpriced this hour · opus-5 100%"), line)
        XCTAssertTrue(line.hasSuffix("102 unpriced today"), line)
    }

    /// Round two, finding 4. The `/hr` segment is keyed on the HOUR's own
    /// state, and when the hour cannot be priced the line says so about the
    /// hour.
    ///
    /// The bug was that the doc justified dropping the rate with "the
    /// `N unpriced` clause already says why" — a clause computed from `today`.
    /// `lastHour` is the trailing 60 minutes and `today` is the server's local
    /// calendar day, so at 00:30 they share no traffic at all: `today` is empty
    /// (a measured `$0.00`, and zero unpriced requests) while the hour still
    /// holds the previous evening's traffic on a model this build has no rate
    /// for. The header then printed `$0.00 today · cache n/a` — the burn rate
    /// the panel exists to show simply gone, with nothing saying it was
    /// unknown rather than unrendered.
    func testAnUnpriceableHourSaysSoEvenWhenTheDayIsEmpty() throws {
        let fleet = Fleet(accounts: [
            row(
                name: "alice@example.com",
                usage: UsageRow(
                    // Midnight has rolled: today served nothing at all.
                    today: totals(cost: nil, requests: 0),
                    window: nil,
                    lastHour: totals(cost: nil, unpriced: 12, requests: 12),
                    todayByModel: [:]))
        ])
        XCTAssertEqual(
            fleet.todayUnpricedRequests, 0,
            "today's count cannot explain the hour — it is a different bucket")
        XCTAssertEqual(fleet.lastHourUnpricedRequests, 12)
        let line = try XCTUnwrap(fleet.usageSummaryLine)
        XCTAssertEqual(line, "$0.00 today · 12 unpriced this hour · cache n/a")
        XCTAssertFalse(
            line.contains("/hr"),
            "no rate is stated — but the hour is no longer silent: \(line)")
    }

    /// Round two, finding 4, the other direction: an hour that served nothing
    /// is a measured `$0.00/hr`, and must not grow an unpriced clause.
    func testAnIdleHourKeepsItsMeasuredZeroRate() throws {
        let idle = totals(cost: nil, requests: 0)
        let fleet = Fleet(accounts: [
            row(
                name: "alice@example.com",
                usage: UsageRow(today: idle, window: nil, lastHour: idle, todayByModel: [:]))
        ])
        XCTAssertEqual(fleet.burnRateSegment, "$0.00/hr")
    }

    // MARK: Formatters — the band is chosen after rounding

    /// Finding 6. `tokens` tested `>= 1_000_000` before rounding, so anything
    /// in [999_500, 999_999] rendered `"1000k"`: five characters on a line this
    /// function exists to keep to four, wearing a unit one band below the
    /// figure it names.
    func testTokensPickTheBandAfterRounding() {
        XCTAssertEqual(QuotaFormat.tokens(999_950), "1.0M")
        XCTAssertEqual(QuotaFormat.tokens(999_500), "1.0M")
        XCTAssertEqual(QuotaFormat.tokens(999_499), "999k", "and below the rounding point it does not")
        XCTAssertEqual(QuotaFormat.tokens(999_500_000), "1.0G")
        XCTAssertEqual(QuotaFormat.tokens(999), "999")
        XCTAssertEqual(QuotaFormat.tokens(1_000), "1.0k")
    }

    /// Finding 6, the same shape in `usd`: `99.96` rounded to `"$100.0"` and
    /// `9.996` to `"$10.00"` — three useful figures where the rule promises
    /// two, on exactly the values sitting under a band's ceiling.
    func testUsdPicksTheBandAfterRounding() {
        XCTAssertEqual(QuotaFormat.usd(99.96), "$100")
        XCTAssertEqual(QuotaFormat.usd(9.996), "$10.0")
        XCTAssertEqual(QuotaFormat.usd(99.94), "$99.9", "and below the rounding point it does not")
        XCTAssertEqual(QuotaFormat.usd(9.994), "$9.99")
    }

    /// Round two, finding 6. The band re-pass fixed the SUFFIX and left the
    /// DECIMALS reading the raw unit, so `9_999` still printed `"10.0k"` —
    /// five characters and three useful figures, the identical defect the
    /// re-pass exists to remove, one field over. Both come from the rounded
    /// figure now.
    func testTokensPickTheirDecimalsAfterRoundingToo() {
        XCTAssertEqual(QuotaFormat.tokens(9_999), "10k", "not 10.0k")
        XCTAssertEqual(QuotaFormat.tokens(9_999_999), "10M")
        XCTAssertEqual(QuotaFormat.tokens(9_999_999_999), "10G")
        XCTAssertEqual(
            QuotaFormat.tokens(9_950), "9.9k",
            "and a value that does NOT round up to ten keeps its decimal")
        XCTAssertEqual(QuotaFormat.tokens(9_949), "9.9k")
        XCTAssertEqual(QuotaFormat.tokens(10_000), "10k")
        XCTAssertEqual(
            QuotaFormat.tokens(9_999).count, 3,
            "the whole rule is four characters, and this is the value that broke it")
    }

    /// Round two, finding 7. The promotion loop stopped at `G`, so the exact
    /// `"1000k"` overflow the re-pass removes everywhere else was still
    /// reachable at the top of the ladder: `999_500_000_000` rendered
    /// `"1000G"`, a unit below the figure it names.
    func testTheTopBandRoundsLikeEveryOtherBand() {
        XCTAssertEqual(QuotaFormat.tokens(999_500_000_000), "1.0T", "not 1000G")
        XCTAssertEqual(QuotaFormat.tokens(5_000_000_000_000), "5.0T", "not 5000G")
        XCTAssertEqual(QuotaFormat.tokens(999_499_999_999), "999G", "below the rounding point")
        XCTAssertEqual(QuotaFormat.tokens(1_000_000_000_000), "1.0T")
    }

    /// Round two, finding 9. `usd` and `tokens` each carried their own
    /// round-then-pick-the-band pass, which is exactly how they drifted: the
    /// same defect was fixed in one and left in the other. One helper now, and
    /// this asserts the rule holds identically on both sides of it — a figure
    /// that rounds up across a boundary is printed at the band it rounded INTO,
    /// never the one it started in.
    func testBothFormattersShareTheSameRoundThenBandRule() {
        // Money: the boundary is a decimal count.
        XCTAssertEqual(QuotaFormat.usd(9.996), "$10.0")
        // Tokens: the boundary is a decimal count AND a unit.
        XCTAssertEqual(QuotaFormat.tokens(9_999), "10k")
        XCTAssertEqual(QuotaFormat.tokens(999_950), "1.0M")
        // And the helper itself, driven directly: one re-pass, not zero.
        let decimals = QuotaFormat.bandAfterRounding(
            9.996,
            band: { $0 < 10 ? 2 : 1 },
            printedMagnitude: { decimals, magnitude in
                Double(String(format: "%.\(decimals)f", magnitude)) ?? magnitude
            })
        XCTAssertEqual(decimals, 1, "9.996 rounds to 10.00, which is the one-decimal band")
    }

    // MARK: A decoded field nobody renders

    /// Finding 8. `Account.cacheCreationTokens` was decoded, initialized and
    /// asserted on, and read by nothing: no view ever drew it, while its
    /// doc-comment promised a reader could "see how much input was served FROM
    /// cache and how much was spent putting it there". This repo's CLAUDE.md
    /// tells the next agent to trust that doc-comment. Cache creation lives on
    /// `UsageTotals`, where it IS rendered.
    func testTheRowCarriesNoFieldNoViewRenders() throws {
        let fleet = try mixedFleet()
        let alice = try account(named: "alice@example.com", in: fleet)
        let stored = Mirror(reflecting: alice).children.compactMap(\.label)
        XCTAssertFalse(
            stored.contains("cacheCreationTokens"),
            "a field the panel never draws is a promise the panel does not keep: \(stored)")
        XCTAssertEqual(
            alice.usage?.today.cacheCreationTokens, 1_600_000,
            "the quantity itself is not lost — it is on the usage bucket, which is read")
    }

    // MARK: Builders

    private func totals(
        cost: Double?,
        input: Int = 0,
        output: Int = 0,
        cacheRead: Int = 0,
        unpriced: Int = 0,
        requests: Int = 1
    ) -> UsageTotals {
        UsageTotals(
            requests: requests,
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
