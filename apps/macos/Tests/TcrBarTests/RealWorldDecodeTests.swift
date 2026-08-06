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
        let json = realWorldFixture
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
}
