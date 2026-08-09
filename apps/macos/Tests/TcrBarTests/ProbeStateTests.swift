import XCTest

@testable import TcrBarCore

/// The panel has to tell two causes of "no quota reading" apart: nothing has
/// probed this account yet, or a probe ran and failed. They look identical in
/// the data (`quota == nil` either way) and they are not interchangeable to a
/// reader — one says wait, the other says look at why.
///
/// Fixtures use obviously-fake account names only. Real account emails never
/// enter this repository — see CLAUDE.md.
final class ProbeStateTests: XCTestCase {

    // MARK: - isFailure

    func testFailureStatesAreFailures() {
        for state in [ProbeState.error, .timeout, .rateLimited] {
            XCTAssertTrue(state.isFailure, "\(state.token) is a probe that ran and failed")
        }
    }

    func testNonFailureStatesAreNotFailures() {
        for state in [ProbeState.never, .ok] {
            XCTAssertFalse(state.isFailure, "\(state.token) is not a failure")
        }
    }

    /// An unrecognised token is a state this build cannot name, which is what
    /// `Tok.unknown` is for. Calling it a failure would assert an observation
    /// nobody made — and because the panel branches on `isFailure` to pick the
    /// pill's WORD, getting this wrong would put an invented cause on screen.
    func testUnknownTokenIsNotAFailure() {
        XCTAssertFalse(ProbeState(token: "some-future-state").isFailure)
        XCTAssertFalse(ProbeState.unknown("some-future-state").isFailure)
    }

    /// `isFailure` must be a strict subset of `hasBeenProbed`: something that
    /// failed necessarily ran. If these ever disagree the panel could claim a
    /// probe failed on an account nothing has asked about.
    func testFailureImpliesProbed() {
        let all: [ProbeState] = [.never, .ok, .error, .timeout, .rateLimited, .unknown("x")]
        for state in all where state.isFailure {
            XCTAssertTrue(
                state.hasBeenProbed,
                "\(state.token) reports a failure but claims never to have been probed")
        }
        XCTAssertTrue(ProbeState.ok.hasBeenProbed, "ok is probed but not a failure")
        XCTAssertFalse(ProbeState.ok.isFailure)
    }

    // MARK: - The regression, at the two predicates the row branches on

    /// The shape observed live: a probe that errored, leaving no quota. Both
    /// predicates the row reads must point at "failed", not at "never asked" —
    /// this account rendered as UNMEASURED, the one word the palette reserves
    /// for never-probed, beside a status of `error`.
    func testErroredProbeWithNoQuotaReadsAsFailedNotUnmeasured() throws {
        let json = """
            [{
              "name": "carol@example.com", "priority": 0, "status": "error",
              "disabled": false, "quota": null, "quotaState": "ok",
              "fiveHour": null, "sevenDay": null, "sevenDayOi": 0.0, "held": [],
              "requests": 0, "inputTokens": 0, "outputTokens": 0,
              "cacheReadTokens": 0, "cacheHitRatio": null,
              "probeStatus": "error", "probeError": "connection refused",
              "lastStreamError": null, "streamErrorCount": 0,
              "source": "live", "serverSha": "abc1234", "serverDirty": false
            }]
            """
        let fleet = try Fleet.decode(Data(json.utf8))
        let account = try XCTUnwrap(fleet.accounts.first)

        XCTAssertFalse(account.hasQuotaEvidence, "a failed probe yields no usable reading")
        XCTAssertTrue(account.probeStatus.isFailure, "the row must be able to name the cause")
        XCTAssertEqual(account.probeStatus.token, "error", "the pill renders this word")
        XCTAssertEqual(account.probeError, "connection refused")
    }

    /// The genuinely-never-probed account must still take the other branch, or
    /// the fix would simply relabel one wrong word with another.
    func testNeverProbedAccountIsNotReportedAsAFailure() throws {
        let json = """
            [{
              "name": "dave@example.com", "priority": 0, "status": "active",
              "disabled": false, "quota": null, "quotaState": "ok",
              "fiveHour": null, "sevenDay": null, "sevenDayOi": 0.0, "held": [],
              "requests": 0, "inputTokens": 0, "outputTokens": 0,
              "cacheReadTokens": 0, "cacheHitRatio": null,
              "probeStatus": "never", "probeError": null,
              "lastStreamError": null, "streamErrorCount": 0,
              "source": "live", "serverSha": "abc1234", "serverDirty": false
            }]
            """
        let fleet = try Fleet.decode(Data(json.utf8))
        let account = try XCTUnwrap(fleet.accounts.first)

        XCTAssertFalse(account.hasQuotaEvidence)
        XCTAssertFalse(account.probeStatus.isFailure, "nothing has asked yet — that is not a failure")
    }
}
