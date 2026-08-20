import XCTest

@testable import TcrBarCore

/// Decode tests whose fixture is real `tcr status --json` output rather than a
/// hand-written shape.
///
/// This file exists because of a specific, shipped failure. The original model was
/// derived by reading row `[0]` of one live sample and typing every field that
/// happened to be non-null there as non-optional. Four fields — `quota`,
/// `fiveHour`, `sevenDay`, `sevenDayOi` — are null on a never-probed account, and
/// the app died with:
///
///     DecodingError.valueNotFound: Expected value of type Double but found null
///     instead. Path: [2].quota
///
/// A hand-authored fixture cannot catch that class of bug, because it encodes the
/// same assumption the model does. Only output the program did not invent can.
/// The rows below are verbatim `tcr status --json`, with ONLY the account names
/// replaced (this repository is public and real account addresses never enter it).
final class RealWorldDecodeTests: XCTestCase {

    /// Three real shapes: a populated account, the never-probed account whose four
    /// quota fractions are all null, and one holding a `7d` window.
    private let realWorldFixture = #"""
        [
          {
            "cacheHitRatio": 0.8434839920081313,
            "cacheReadTokens": 7407414,
            "disabled": false,
            "fiveHour": 0.04,
            "held": [],
            "inputTokens": 8781926,
            "lastStreamError": null,
            "name": "alice@example.com",
            "outputTokens": 31860,
            "priority": 0,
            "probeError": null,
            "probeStatus": "ok",
            "quota": 0.04,
            "quotaState": "ok",
            "requests": 102,
            "serverDirty": false,
            "serverSha": "bd60839",
            "sevenDay": 0.01,
            "sevenDayOi": 0.0,
            "source": "live",
            "status": "active",
            "streamErrorCount": 0
          },
          {
            "cacheHitRatio": null,
            "cacheReadTokens": 0,
            "disabled": true,
            "fiveHour": null,
            "held": [],
            "inputTokens": 0,
            "lastStreamError": null,
            "name": "bob@example.com",
            "outputTokens": 0,
            "priority": 10,
            "probeError": null,
            "probeStatus": "never",
            "quota": null,
            "quotaState": "ok",
            "requests": 0,
            "serverDirty": false,
            "serverSha": "bd60839",
            "sevenDay": null,
            "sevenDayOi": null,
            "source": "live",
            "status": "active",
            "streamErrorCount": 0
          },
          {
            "cacheHitRatio": null,
            "cacheReadTokens": 0,
            "disabled": false,
            "fiveHour": 0.0,
            "held": [
              {
                "minutesUntilReset": 6498,
                "resetAtMs": 1786406400224,
                "window": "7d"
              }
            ],
            "inputTokens": 0,
            "lastStreamError": null,
            "name": "carol@example.com",
            "outputTokens": 0,
            "priority": 0,
            "probeError": null,
            "probeStatus": "ok",
            "quota": 0.99,
            "quotaState": "near",
            "requests": 0,
            "serverDirty": false,
            "serverSha": "bd60839",
            "sevenDay": 0.99,
            "sevenDayOi": 0.0,
            "source": "live",
            "status": "active",
            "streamErrorCount": 0
          }
        ]
        """#

    private func fleet() throws -> Fleet {
        try Fleet.decode(Data(realWorldFixture.utf8))
    }

    /// The regression: this exact payload used to throw and blank the whole panel.
    func testRealWorldOutputDecodesWithoutThrowing() throws {
        let fleet = try fleet()
        XCTAssertEqual(fleet.accounts.count, 3)
        XCTAssertTrue(
            fleet.unreadable.isEmpty,
            "every row in real output must decode; unreadable: \(fleet.unreadable)"
        )
    }

    /// `realWorldFixture` predates `fiveHourResetAtMs`/`sevenDayResetAtMs` — it
    /// has neither key, the shape an older `tcr` this newer TcrBar talks to
    /// would still emit. Both fields must decode to `nil`, not throw.
    func testMissingResetAtMsFieldsDecodeToNilNotAThrow() throws {
        let fleet = try fleet()
        XCTAssertTrue(
            fleet.unreadable.isEmpty,
            "an older tcr without the reset fields must still decode every row"
        )
        for account in fleet.accounts {
            XCTAssertNil(account.fiveHourResetAtMs)
            XCTAssertNil(account.sevenDayResetAtMs)
        }
    }

    /// One null field must not cost the other rows. Before per-row decoding, the
    /// array decoded atomically and a single null erased all thirteen accounts.
    func testTheNeverProbedRowKeepsItsNullsAndCostsNoOtherRow() throws {
        let fleet = try fleet()
        let neverProbed = fleet.accounts[1]

        XCTAssertNil(neverProbed.quota)
        XCTAssertNil(neverProbed.fiveHour)
        XCTAssertNil(neverProbed.sevenDay)
        XCTAssertNil(neverProbed.sevenDayOi)
        XCTAssertEqual(neverProbed.probeStatus, .never)

        // The neighbours are untouched.
        XCTAssertEqual(fleet.accounts[0].quota, 0.04)
        XCTAssertEqual(fleet.accounts[2].quota, 0.99)
    }

    /// `quotaState` is `"ok"` on that row, but nothing has ever measured it. Counting
    /// it as capacity is the overclaim the whole optional model exists to prevent —
    /// and the in-app enable button makes it reachable.
    ///
    /// The live row is `disabled: true`, so asserting `isReady == false` on it as-is
    /// is VACUOUS: `!disabled` short-circuits and the assertion passes even with the
    /// evidence rule deleted. Verified — removing `&& hasQuotaEvidence` from
    /// `isReady` left this file green. So the readiness half of the rule is exercised
    /// against the same row ENABLED, which is precisely the state the in-app enable
    /// button produces.
    func testNeverProbedIsNotEvidence() throws {
        let neverProbed = try fleet().accounts[1]

        XCTAssertEqual(neverProbed.quotaState, .ok, "the raw field really does say ok")
        XCTAssertFalse(neverProbed.hasQuotaEvidence, "but nothing has ever probed it")
    }

    func testEnablingANeverProbedAccountDoesNotCreateCapacity() throws {
        let enabled = try enabledNeverProbedFleet()
        let account = enabled.accounts[0]

        XCTAssertFalse(account.disabled, "precondition: the short-circuit is not in play")
        XCTAssertEqual(account.quotaState, .ok)
        XCTAssertFalse(
            account.isReady,
            "an enabled account nothing has ever probed is not capacity"
        )
        XCTAssertEqual(enabled.readyCount, 0, "and it must not inflate the summary")
    }

    /// The never-probed row with `disabled` flipped to `false` — the exact state the
    /// panel's enable button puts it in.
    private func enabledNeverProbedFleet() throws -> Fleet {
        let json =
            realWorldFixture
            .replacingOccurrences(of: "\"disabled\": true", with: "\"disabled\": false")
        let all = try Fleet.decode(Data(json.utf8))
        let neverProbed = try XCTUnwrap(all.accounts.first { $0.probeStatus == .never })
        return Fleet(accounts: [neverProbed])
    }

    /// A `7d` hold survives the round trip with both halves of its reset intact.
    func testHeldWindowSurvivesRealOutput() throws {
        let holder = try fleet().accounts[2]
        let hold = try XCTUnwrap(holder.soonestHold)

        XCTAssertEqual(hold.window, "7d")
        XCTAssertEqual(hold.minutesUntilReset, 6498)
        XCTAssertEqual(hold.resetAtMs, 1_786_406_400_224)
    }

    /// Rows are ordered by "can this serve me", not by rotation priority alone.
    ///
    /// The fixture is rewritten to give the DISABLED account the best priority,
    /// because on the unmodified data both orderings happen to agree — asserting
    /// against that would be a test that cannot fail. The final assertion pins the
    /// discrimination itself: a priority-only sort really does lead with the parked
    /// account.
    func testParkedAccountSinksBelowUsableOnesWhateverItsPriority() throws {
        let json = realWorldFixture.replacingOccurrences(
            of: "\"priority\": 10",
            with: "\"priority\": -1"
        )
        let fleet = try Fleet.decode(Data(json.utf8))

        XCTAssertEqual(
            fleet.accounts.first(where: \.disabled)?.priority, -1,
            "precondition: the parked account holds the best priority"
        )
        XCTAssertEqual(
            fleet.rowsInDisplayOrder.map(\.name),
            ["alice@example.com", "carol@example.com", "bob@example.com"],
            "usable first (ok, then near), parked last"
        )
        XCTAssertEqual(
            fleet.accounts.sorted(by: { $0.priority < $1.priority }).first?.name,
            "bob@example.com",
            "control: the old priority-only sort would have led with the parked account"
        )
    }

    // MARK: The committed cross-language contract fixture

    /// `<repo>/tests/fixtures/status-contract.json` — the SAME file the Rust test
    /// `cli::tests::status_contract_fixture_matches_committed` renders and
    /// compares against, not a copy of it. Two copies that must stay equal are
    /// precisely the drift this pair of tests exists to prevent.
    ///
    /// Located from `#filePath` rather than from a bundled resource: the fixture
    /// lives outside the SwiftPM package (`apps/macos/`), so it cannot be
    /// declared as a target resource without copying it into the package tree.
    /// `#filePath` is this file's own absolute path at compile time, so five
    /// `deletingLastPathComponent`s — the file, `TcrBarTests`, `Tests`, `macos`,
    /// `apps` — land on the repository root.
    static var contractFixtureURL: URL {
        var url = URL(fileURLWithPath: #filePath)
        for _ in 0..<5 { url = url.deletingLastPathComponent() }
        return url.appendingPathComponent("tests/fixtures/status-contract.json")
    }

    private func contractFixtureData() throws -> Data {
        let url = Self.contractFixtureURL
        XCTAssertTrue(
            FileManager.default.fileExists(atPath: url.path),
            "the committed contract fixture must be reachable from the Swift package: \(url.path)"
        )
        return try Data(contentsOf: url)
    }

    /// THE OTHER HALF OF THE CONTRACT PIN.
    ///
    /// The Rust side proves the renderer still emits these exact bytes; this
    /// proves this app can still read them. A key renamed in
    /// `render_accounts_json` turns the Rust test red, and regenerating the
    /// fixture to satisfy it turns this one red — which is the only arrangement
    /// in which a silent rename is impossible.
    func testCommittedContractFixtureDecodes() throws {
        let fleet = try Fleet.decode(contractFixtureData())

        XCTAssertTrue(
            fleet.unreadable.isEmpty,
            "every row of the committed contract must decode; unreadable: \(fleet.unreadable)"
        )
        XCTAssertEqual(fleet.accounts.count, 4)
        XCTAssertEqual(
            fleet.accounts.map(\.name),
            [
                "alice@example.com", "bob@example.com", "carol@example.com", "dave@example.com",
            ]
        )
    }

    /// The load-bearing fields, per row. A decode that succeeds while every
    /// value lands as a default would still pass ``testCommittedContractFixtureDecodes``,
    /// so each key the panel actually reads is checked for its value here.
    func testCommittedContractFixtureCarriesTheFieldsThePanelReads() throws {
        let fleet = try Fleet.decode(contractFixtureData())
        let byName = Dictionary(uniqueKeysWithValues: fleet.accounts.map { ($0.name, $0) })

        let ok = try XCTUnwrap(byName["alice@example.com"])
        XCTAssertEqual(ok.quotaState, .ok)
        XCTAssertEqual(ok.probeStatus, .ok)
        XCTAssertEqual(ok.quota, 0.04)
        XCTAssertEqual(ok.fiveHour, 0.04)
        XCTAssertEqual(ok.sevenDay, 0.01)
        XCTAssertEqual(ok.sevenDayOi, 0.0)
        XCTAssertEqual(ok.cacheHitRatio, 0.75)
        XCTAssertEqual(ok.requests, 102)
        XCTAssertEqual(ok.inputTokens, 8_000_000)
        XCTAssertEqual(ok.outputTokens, 31_860)
        XCTAssertEqual(ok.cacheReadTokens, 6_000_000)
        XCTAssertEqual(ok.streamErrorCount, 2)
        XCTAssertEqual(ok.lastStreamError, "overloaded_error")
        XCTAssertEqual(ok.priority, 0)
        XCTAssertEqual(ok.status, "active")
        XCTAssertFalse(ok.disabled)
        XCTAssertEqual(ok.source, .live)
        XCTAssertEqual(ok.serverSha, "abc1234")
        XCTAssertEqual(ok.serverDirty, false)
        XCTAssertTrue(ok.hasQuotaEvidence)
        XCTAssertTrue(ok.isReady)
        XCTAssertTrue(ok.held.isEmpty)
        // A row WITH the new per-window reset fields populates them.
        XCTAssertEqual(ok.fiveHourResetAtMs, 1_767_225_600_000)
        XCTAssertEqual(ok.sevenDayResetAtMs, 1_767_312_000_000)

        // `near`: the only shape carrying the nested `held` objects.
        let near = try XCTUnwrap(byName["bob@example.com"])
        XCTAssertEqual(near.quotaState, .near)
        XCTAssertEqual(near.probeStatus, .rateLimited)
        XCTAssertEqual(near.held.map(\.window), ["5h", "7d"])
        let hold = try XCTUnwrap(near.soonestHold)
        XCTAssertEqual(hold.resetAtMs, 1_767_225_600_000)
        XCTAssertEqual(hold.minutesUntilReset, 0)
        XCTAssertFalse(near.isReady, "a held account is not capacity")

        // `spent`, and the only row carrying a probe error string.
        let spent = try XCTUnwrap(byName["carol@example.com"])
        XCTAssertEqual(spent.quotaState, .spent)
        XCTAssertEqual(spent.probeStatus, .error)
        XCTAssertEqual(spent.probeError, "probe failed: connection reset")
        XCTAssertEqual(spent.status, "throttled")
        XCTAssertTrue(spent.hasQuotaEvidence, "a failed probe keeps the last-learned bar")

        // Never probed and disabled: five nulls, and none of them may render as
        // a measured zero.
        let never = try XCTUnwrap(byName["dave@example.com"])
        XCTAssertEqual(never.probeStatus, .never)
        XCTAssertTrue(never.disabled)
        XCTAssertNil(never.quota)
        XCTAssertNil(never.fiveHour)
        XCTAssertNil(never.sevenDay)
        XCTAssertNil(never.sevenDayOi)
        XCTAssertNil(never.cacheHitRatio)
        XCTAssertNil(never.lastStreamError)
        XCTAssertNil(never.probeError)
        XCTAssertEqual(never.quotaState, .ok, "the raw field says ok — it is a Rust default")
        XCTAssertFalse(never.hasQuotaEvidence, "but nothing has ever probed it")

        // The fleet aggregates the panel headline reads.
        XCTAssertEqual(fleet.readyCount, 1)
        XCTAssertEqual(fleet.enabledCount, 3)
        XCTAssertEqual(fleet.unmeasuredCount, 0, "every ENABLED row here has been probed")
        XCTAssertEqual(fleet.source, .live)
        XCTAssertEqual(fleet.serverSha, "abc1234")
    }

    /// The `unknown` variant, which the committed fixture cannot contain.
    ///
    /// `quota_state_token` (src/cli.rs) emits exactly `ok` / `near` / `spent`,
    /// so a genuine renderer output can never carry a fourth token — the whole
    /// point of ``QuotaState/unknown(_:)`` is to survive a token a *future* Rust
    /// build invents. That future is simulated by rewriting the committed
    /// bytes, so the forward-compatibility rule is exercised against the real
    /// contract rather than against a hand-written shape.
    func testAFutureQuotaStateDegradesInsteadOfBlankingTheRow() throws {
        let json = try XCTUnwrap(String(data: contractFixtureData(), encoding: .utf8))
            .replacingOccurrences(of: "\"quotaState\": \"spent\"", with: "\"quotaState\": \"parked\"")
        let fleet = try Fleet.decode(Data(json.utf8))

        XCTAssertTrue(
            fleet.unreadable.isEmpty,
            "an unseen quotaState must never cost a row: \(fleet.unreadable)"
        )
        let future = try XCTUnwrap(fleet.accounts.first { $0.name == "carol@example.com" })
        XCTAssertEqual(future.quotaState, .unknown("parked"))
        XCTAssertEqual(future.quotaState.token, "parked", "the raw text stays displayable")
        XCTAssertFalse(future.isReady, "an unnameable state is never counted as capacity")
    }

    /// The same rule for `probeStatus`, whose token set is owned by
    /// `ProbeStatus::as_str` (src/probe.rs) and is the one the capacity summary
    /// reads through ``ProbeState/hasBeenProbed``.
    func testAFutureProbeStatusDegradesToNotEvidence() throws {
        let json = try XCTUnwrap(String(data: contractFixtureData(), encoding: .utf8))
            .replacingOccurrences(of: "\"probeStatus\": \"ok\"", with: "\"probeStatus\": \"queued\"")
        let fleet = try Fleet.decode(Data(json.utf8))

        XCTAssertTrue(fleet.unreadable.isEmpty, "\(fleet.unreadable)")
        let future = try XCTUnwrap(fleet.accounts.first { $0.name == "alice@example.com" })
        XCTAssertEqual(future.probeStatus, .unknown("queued"))
        XCTAssertFalse(
            future.probeStatus.hasBeenProbed,
            "an unseen probe state is not evidence — understating capacity is the safe direction"
        )
        XCTAssertFalse(future.hasQuotaEvidence, "so the row stops counting as capacity")
    }
}
