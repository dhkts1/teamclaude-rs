import XCTest

@testable import TcrBarCore

/// One poll runs two reads at once and folds them with
/// ``StatusPoller/combine(accounts:sessions:)``. This is the fold.
///
/// It is tested on its own because the rule it carries used to be the shape of
/// a control flow: the sessions read was reached by falling through the
/// accounts branch, so "the second read may only ever ADD" held as long as
/// nobody moved an early return. Both halves now start together and the rule
/// has one function, so it also gets a test that goes red when the rule is
/// dropped.
///
/// Fixtures use obviously-fake account names only. Real account data never
/// enters this repository, see CLAUDE.md.
final class StatusPollerCombineTests: XCTestCase {
    private func sessionsRead(_ json: String) throws -> Fleet.SessionsRead {
        try Fleet.decodeSessions(Data(json.utf8))
    }

    private func accountsFleet() throws -> Fleet {
        try Fleet.decode(
            Data(
                """
                [{"name":"alice@example.com","priority":1,"status":"active","disabled":false,
                  "quota":0.42,"quotaState":"ok","fiveHour":0.11,"sevenDay":0.42,
                  "held":[],"probeStatus":"ok","probeError":null,
                  "lastStreamError":null,"streamErrorCount":0,
                  "requests":3,"inputTokens":1,"outputTokens":1,"cacheReadTokens":1,
                  "cacheCreationTokens":0,"cacheHitRatio":0.5,"source":"live",
                  "serverSha":"abc1234","serverDirty":false,
                  "usage":{"today":{"requests":1,"inputTokens":1,"cacheCreationTokens":0,
                    "cacheCreation1hTokens":0,"cacheReadTokens":1,"outputTokens":1,
                    "costUsd":0.42,"unpricedRequests":0},
                   "window":{"requests":1,"inputTokens":1,"cacheCreationTokens":0,
                    "cacheCreation1hTokens":0,"cacheReadTokens":1,"outputTokens":1,
                    "costUsd":0.42,"unpricedRequests":0,"since":1767207600000},
                   "lastHour":{"requests":1,"inputTokens":1,"cacheCreationTokens":0,
                    "cacheCreation1hTokens":0,"cacheReadTokens":1,"outputTokens":1,
                    "costUsd":0.42,"unpricedRequests":0},
                   "todayByModel":{}}}]
                """.utf8))
    }

    /// A read that decoded takes the sessions half with it.
    func testALoadedReadCarriesTheSessionsHalf() throws {
        let read = try sessionsRead(#"{"supported": true, "sessions": []}"#)
        let combined = StatusPoller.combine(
            accounts: .loaded(try accountsFleet()), sessions: read)
        guard case .loaded(let fleet) = combined else {
            return XCTFail("a loaded accounts half must stay loaded, got \(combined)")
        }
        XCTAssertEqual(fleet.accounts.count, 1)
        XCTAssertTrue(
            fleet.sessionsSupported,
            "the sessions channel the second read established must reach the fleet")
    }

    /// The containment rule, in the direction that matters: a sessions read
    /// that succeeded may not dress up an accounts read that failed. Both
    /// children now run whatever the other does, so this is the branch that
    /// stops one answering for the other.
    func testAFailedAccountsReadIsPublishedUntouched() throws {
        let read = try sessionsRead(#"{"supported": true, "sessions": []}"#)
        let failed = PollState.commandFailed(exitCode: 1, message: "no server")
        XCTAssertEqual(
            StatusPoller.combine(accounts: failed, sessions: read), failed,
            "a poll whose accounts half failed publishes that failure and nothing else")

        let missing = PollState.toolMissing(searched: ["/usr/local/bin/tcr"])
        XCTAssertEqual(StatusPoller.combine(accounts: missing, sessions: read), missing)

        let undecodable = PollState.undecodable(message: "not JSON")
        XCTAssertEqual(StatusPoller.combine(accounts: undecodable, sessions: read), undecodable)
    }

    /// And the other direction: a sessions half that failed costs the Sessions
    /// and Tools tabs, never the accounts the panel is mostly made of.
    func testAFailedSessionsHalfStillPublishesTheAccounts() throws {
        let combined = StatusPoller.combine(
            accounts: .loaded(try accountsFleet()),
            sessions: Fleet.SessionsRead(channel: .commandFailed("no server")))
        guard case .loaded(let fleet) = combined else {
            return XCTFail("the accounts half decoded, so the poll is loaded, got \(combined)")
        }
        XCTAssertEqual(fleet.accounts.count, 1)
        XCTAssertFalse(fleet.sessionsSupported)
    }
}
