import XCTest

@testable import TcrBarCore

/// One person holding two orgs — a personal Max org and a company Team org.
/// Everything here guards the consequences of that, which were all real: two
/// rows collapsing into one in the panel, and every per-account command refusing
/// as ambiguous.
///
/// The fixture carries the names `tcr` gives that fleet: the personal row keeps
/// the bare email, the Team row is `email/<org-slug>`. That IS the fix — the
/// panel's job is now to key on those names and not undo it.
final class AccountIdentityTests: XCTestCase {

    private let personal = "11111111-1111-1111-1111-111111111111"
    private let team = "22222222-2222-2222-2222-222222222222"

    // MARK: - identity

    /// The defect itself: `ForEach(…, id: \.element.id)` treated the pair as one
    /// row, so the panel drew the first row's numbers on both and neither wore
    /// its own gate pill, while `tcr status --json` reported them correctly and
    /// differently.
    func testTheTwoRowsOfOnePersonsTwoOrgsHaveDistinctIds() throws {
        let fleet = try Fleet.decode(Data(duplicateEmailJSON.utf8))
        XCTAssertEqual(fleet.accounts.count, 2)

        let ids = Set(fleet.accounts.map(\.id))
        XCTAssertEqual(
            ids.count, 2,
            "two rows must be two identities — one id means the panel renders one "
                + "row's numbers twice"
        )
        XCTAssertEqual(
            fleet.accounts.map(\.name),
            ["henry@example.com", "henry@example.com/example-team"],
            "the control: the ids differ because the NAMES differ, which is what "
                + "tcr guarantees — not because this app added something to them"
        )
    }

    /// The row draws a name on two lines — the email, then the `/org` half as
    /// its own tag — so that neither has to be truncated. The split must lose
    /// nothing: reading the two halves left to right has to reproduce the name
    /// byte-for-byte, because that string is what the operator types back.
    ///
    /// The third case is the one worth having: a name with a SECOND separator
    /// keeps the whole remainder in the tag. Splitting on every `/` would drop
    /// `/y` on the floor and quietly display a name that addresses nothing.
    func testDisplayHalvesSplitAtTheFirstSeparatorAndLoseNothing() {
        let bare = AccountRef(name: "alice@example.com").displayHalves
        XCTAssertEqual(bare.email, "alice@example.com")
        XCTAssertNil(bare.orgTag, "a bare email has no tag — that row renders as it always has")

        let qualified = AccountRef(name: "alice@example.com/acme").displayHalves
        XCTAssertEqual(qualified.email, "alice@example.com")
        XCTAssertEqual(
            qualified.orgTag, "/acme",
            "the tag carries the separator, so the two halves concatenate back to the name"
        )

        let pathological = AccountRef(name: "alice@example.com/x/y").displayHalves
        XCTAssertEqual(pathological.email, "alice@example.com")
        XCTAssertEqual(
            pathological.orgTag, "/x/y",
            "a second separator stays INSIDE the tag — splitting on it would drop /y"
        )

        // The property behind all three, stated once: nothing is lost.
        for name in [
            "alice@example.com",
            "alice@example.com/acme",
            "alice@example.com/x/y",
            "alice@example.com/",
        ] {
            let halves = AccountRef(name: name).displayHalves
            XCTAssertEqual(
                halves.email + (halves.orgTag ?? ""), name,
                "the two halves must reassemble into exactly the name"
            )
        }
    }

    /// A row's identity is its name, with nothing appended: the `id` a
    /// dictionary is keyed by and the string handed to `tcr` are the same bytes,
    /// so a key can never be built that `tcr` would not accept.
    func testIdIsExactlyTheName() {
        XCTAssertEqual(AccountRef(name: "alice@example.com").id, "alice@example.com")
        XCTAssertEqual(
            AccountRef(name: "alice@example.com/acme").id, "alice@example.com/acme",
            "a qualified name is passed through untouched — no separator of our own"
        )
    }

    /// The row's `id` and the key its verdict is filed under must be the same
    /// string, because the drift between them IS the bug.
    func testAccountIdMatchesItsRefsId() throws {
        let fleet = try Fleet.decode(Data(duplicateEmailJSON.utf8))
        for account in fleet.accounts {
            XCTAssertEqual(account.id, account.ref.id)
        }
    }

    /// A verdict recorded against one row must not appear on the other. Keyed by
    /// name, both rows showed it.
    @MainActor
    func testAVerdictOnOneRowDoesNotSurfaceOnItsTwin() async throws {
        let fleet = try Fleet.decode(Data(duplicateEmailJSON.utf8))
        let first = fleet.accounts[0].ref
        let second = fleet.accounts[1].ref

        let controller = AccountController()
        controller.record(
            readback: .loaded(fleet),
            requestedEnabled: true,
            account: first,
            now: Date()
        )

        XCTAssertNotNil(
            controller.verdict(for: first, reportedDisabled: false),
            "the row that was toggled shows its own verdict"
        )
        XCTAssertNil(
            controller.verdict(for: second, reportedDisabled: false),
            "its same-email twin in another org must show nothing"
        )
    }

    // MARK: - decoding

    func testPlanAndOrgKeysDecode() throws {
        let fleet = try Fleet.decode(Data(duplicateEmailJSON.utf8))
        XCTAssertEqual(fleet.accounts[0].plan, "Max 20x")
        XCTAssertEqual(fleet.accounts[0].organizationType, "claude_max")
        XCTAssertEqual(fleet.accounts[0].rateLimitTier, "default_claude_max_20x")
        XCTAssertNil(fleet.accounts[0].seatTier, "a Max org has no seat")
        XCTAssertEqual(fleet.accounts[0].orgUuid, personal)
        XCTAssertEqual(fleet.accounts[0].orgName, "Example Personal")

        XCTAssertEqual(fleet.accounts[1].plan, "Team Standard")
        XCTAssertEqual(fleet.accounts[1].seatTier, "team_standard")
        XCTAssertEqual(fleet.accounts[1].orgUuid, team)
    }

    /// The gate the panel had no way to see. Anthropic's own rejection takes an
    /// account out of selection while `status` stays `"active"` and the quota
    /// bars can look ordinary, so before this decoded the row was drawn as
    /// eligible for traffic the router will never send it.
    func testTheRejectedGateDecodes() throws {
        let fleet = try Fleet.decode(Data(duplicateEmailJSON.utf8))
        XCTAssertEqual(fleet.accounts[0].gate, .rejected)
        XCTAssertTrue(fleet.accounts[0].isRejected)
        XCTAssertFalse(
            fleet.accounts[1].isRejected,
            "the control: the sibling row carries no gate and must not inherit one"
        )
    }

    /// A gate token this build does not know must stay readable rather than
    /// throwing the row away — the same tolerance `QuotaState` has.
    func testAnUnknownGateTokenIsKeptVerbatim() {
        XCTAssertEqual(GateReason(token: "fable-weekly"), .unknown("fable-weekly"))
        XCTAssertEqual(GateReason(token: "fable-weekly").token, "fable-weekly")
        XCTAssertFalse(
            GateReason(token: "five-hour") == .rejected,
            "only `rejected` is rejected — a quota gate is not Anthropic's verdict"
        )
    }

    /// The forward-compat contract every optional field on this row carries: a
    /// server built before these keys existed omits them entirely, and its rows
    /// must still decode rather than throwing the panel back to a fabricated
    /// offline snapshot.
    func testARowWithNoPlanOrOrgKeysStillDecodes() throws {
        let fleet = try Fleet.decode(Data(planlessJSON.utf8))
        let account = try XCTUnwrap(fleet.accounts.first)
        XCTAssertNil(account.plan)
        XCTAssertNil(account.organizationType)
        XCTAssertNil(account.orgUuid)
        XCTAssertNil(
            account.gate,
            "absent is `nil`, not `.ok` — this server does not report a gate, which "
                + "is not the same as reporting that there is none"
        )
        XCTAssertFalse(account.isRejected)
        XCTAssertEqual(account.id, "solo@example.com")
    }

    // MARK: - the commands each row issues

    /// Every per-account verb carries the row's NAME and nothing else. The
    /// qualified name is what addresses the Team row, and it must reach `tcr`
    /// byte-for-byte — a panel that stripped the `/example-team` half would
    /// silently act on the personal row instead.
    func testEveryPerAccountCommandCarriesTheRowsFullName() {
        let teamRow = "henry@example.com/example-team"
        XCTAssertEqual(
            TokenCommand.arguments(query: teamRow),
            ["token", teamRow]
        )
        XCTAssertEqual(
            RemoveAccountCommand.arguments(query: teamRow),
            ["remove", teamRow]
        )
        XCTAssertEqual(
            AccountCommand.arguments(enabled: false, name: teamRow),
            ["disable", teamRow]
        )
        XCTAssertEqual(
            AccountCommand.arguments(enabled: true, name: teamRow),
            ["enable", teamRow]
        )
        XCTAssertEqual(
            ControlAccountCommand.setArguments(name: teamRow),
            ["control", teamRow]
        )
        XCTAssertTrue(
            LoginLauncher.script(
                forExecutableAt: "/usr/local/bin/tcr",
                reloggingIn: teamRow
            ).contains("--account '\(teamRow)'"),
            "a re-login names the same row every other verb does"
        )
    }

    /// No verb may build an `--org` flag any more: `tcr` does not take one, so
    /// one here would abort the command outright. This is the panel's half of
    /// the same gate the Rust side keeps.
    func testNoCommandBuildsAnOrgFlag() {
        let built: [[String]] = [
            TokenCommand.arguments(query: "henry@example.com"),
            RemoveAccountCommand.arguments(query: "henry@example.com"),
            AccountCommand.arguments(enabled: true, name: "henry@example.com"),
            AccountCommand.arguments(enabled: false, name: "henry@example.com"),
            ControlAccountCommand.setArguments(name: "henry@example.com"),
            ControlAccountCommand.setArguments(name: nil),
            GroupCommand.addArguments(group: "gil", account: "henry@example.com"),
            GroupCommand.removeArguments(group: "gil", account: "henry@example.com"),
        ]
        for arguments in built {
            XCTAssertFalse(arguments.contains("--org"), "\(arguments) must not narrow by org")
        }
        let script = LoginLauncher.script(
            forExecutableAt: "/usr/local/bin/tcr", reloggingIn: "henry@example.com")
        XCTAssertFalse(script.contains("--org"), script)
    }

    /// The group verbs are the ones Gil hit: `tcr group add` labelled whichever
    /// same-email row came first and reported success. The name it takes now
    /// resolves to one row or to none.
    func testGroupCommandsCarryTheRowsFullName() {
        let teamRow = "henry@example.com/example-team"
        XCTAssertEqual(
            GroupCommand.addArguments(group: "gil", account: teamRow),
            ["group", "add", "gil", teamRow]
        )
        XCTAssertEqual(
            GroupCommand.removeArguments(group: "gil", account: teamRow),
            ["group", "rm", "gil", teamRow]
        )
        XCTAssertEqual(
            GroupCommand.addArguments(group: "gil", account: "solo@example.com"),
            ["group", "add", "gil", "solo@example.com"]
        )
    }

    /// The two rows must not share a group failure or an in-flight spinner, for
    /// the same reason they must not share a toggle verdict.
    func testGroupFailuresAreKeyedPerRow() {
        let personalRef = AccountRef(name: "henry@example.com")
        let teamRef = AccountRef(name: "henry@example.com/example-team")
        XCTAssertNotEqual(
            GroupController.memberKey(group: "gil", account: personalRef),
            GroupController.memberKey(group: "gil", account: teamRef)
        )
    }

    /// A name is shell-quoted the same POSIX way the path already is — it goes
    /// onto a command line in a `.command` file, so unquoted interpolation is
    /// injection. An account name is attacker-adjacent input in principle, and
    /// it now has a slash in it as a matter of course.
    func testTheAccountArgumentIsShellQuoted() {
        let script = LoginLauncher.script(
            forExecutableAt: "/usr/local/bin/tcr",
            reloggingIn: "it's; rm -rf /"
        )
        XCTAssertTrue(script.contains("--account 'it'\\''s; rm -rf /'"), script)
    }

    // MARK: - fixtures

    private var duplicateEmailJSON: String {
        """
        [{"name":"henry@example.com","priority":0,"status":"active","disabled":false,
          "plan":"Max 20x","organizationType":"claude_max",
          "rateLimitTier":"default_claude_max_20x","seatTier":null,
          "orgUuid":"\(personal)","orgName":"Example Personal",
          "gate":"rejected",
          "quota":1.0,"quotaState":"spent","fiveHour":1.0,"sevenDay":1.0,
          "sevenDayOi":null,"held":[],"requests":13011,"inputTokens":1,"outputTokens":1,
          "cacheReadTokens":1,"cacheHitRatio":0.5,"probeStatus":"ok","probeError":null,
          "lastStreamError":null,"streamErrorCount":0,"source":"live",
          "serverSha":"abc1234","serverDirty":false},
         {"name":"henry@example.com/example-team","priority":1,"status":"active","disabled":false,
          "plan":"Team Standard","organizationType":"claude_team",
          "rateLimitTier":"default_raven","seatTier":"team_standard",
          "orgUuid":"\(team)","orgName":"Example Team",
          "quota":null,"quotaState":"ok","fiveHour":null,"sevenDay":null,
          "sevenDayOi":null,"held":[],"requests":0,"inputTokens":0,"outputTokens":0,
          "cacheReadTokens":0,"cacheHitRatio":null,"probeStatus":"never","probeError":null,
          "lastStreamError":null,"streamErrorCount":0,"source":"live",
          "serverSha":"abc1234","serverDirty":false}]
        """
    }

    private var planlessJSON: String {
        """
        [{"name":"solo@example.com","priority":0,"status":"active","disabled":false,
          "quota":0.1,"quotaState":"ok","fiveHour":0.1,"sevenDay":0.1,
          "sevenDayOi":null,"held":[],"requests":1,"inputTokens":1,"outputTokens":1,
          "cacheReadTokens":1,"cacheHitRatio":0.5,"probeStatus":"ok","probeError":null,
          "lastStreamError":null,"streamErrorCount":0,"source":"live",
          "serverSha":"abc1234","serverDirty":false}]
        """
    }
}
