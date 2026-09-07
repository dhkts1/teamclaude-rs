import XCTest

@testable import TcrBarCore

/// The fleet holds one email twice — a personal Max org and a company Team org.
/// Everything here guards the consequences of that, which were all real:
/// two rows collapsing into one in the panel, and every per-account command
/// refusing as ambiguous.
final class AccountIdentityTests: XCTestCase {

    private let personal = "11111111-1111-1111-1111-111111111111"
    private let team = "22222222-2222-2222-2222-222222222222"

    // MARK: - identity

    /// The defect itself: `ForEach(…, id: \.element.id)` treated the pair as one
    /// row, so the panel drew the first row's numbers on both and neither wore
    /// its own gate pill, while `tcr status --json` reported them correctly and
    /// differently.
    func testTwoRowsSharingAnEmailInDifferentOrgsHaveDistinctIds() throws {
        let fleet = try Fleet.decode(Data(duplicateEmailJSON.utf8))
        XCTAssertEqual(fleet.accounts.count, 2)

        let ids = Set(fleet.accounts.map(\.id))
        XCTAssertEqual(
            ids.count, 2,
            "two rows sharing a name must be two identities — one id means the panel "
                + "renders one row's numbers twice"
        )
        XCTAssertEqual(
            Set(fleet.accounts.map(\.name)).count, 1,
            "the control: the two rows really do share a name, so this test is not "
                + "passing because the fixture made them different"
        )
    }

    /// A server that reports no org — an older build — must render exactly as it
    /// does today, and with no org there genuinely is no more identity to have.
    func testIdFallsBackToTheBareNameWithoutAnOrg() {
        XCTAssertEqual(AccountRef(name: "alice@example.com").id, "alice@example.com")
        XCTAssertEqual(
            AccountRef(name: "alice@example.com", orgUuid: "").id, "alice@example.com",
            "an empty string is not an org — it must not produce a trailing separator"
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
        XCTAssertEqual(account.id, "solo@example.com")
    }

    // MARK: - the commands each row issues

    /// Every per-account verb passes `--org` when the row has one. Without it
    /// `tcr` refuses: `'…' is ambiguous — matches 2 accounts … Narrow with
    /// --org`, which is what "Copy Access Token" failed with on this fleet.
    func testEveryPerAccountCommandCarriesTheOrgWhenThereIsOne() {
        XCTAssertEqual(
            TokenCommand.arguments(query: "henry@example.com", org: personal),
            ["token", "henry@example.com", "--org", personal]
        )
        XCTAssertEqual(
            RemoveAccountCommand.arguments(query: "henry@example.com", org: personal),
            ["remove", "henry@example.com", "--org", personal]
        )
        XCTAssertEqual(
            AccountCommand.arguments(enabled: false, name: "henry@example.com", org: personal),
            ["disable", "henry@example.com", "--org", personal]
        )
        XCTAssertEqual(
            AccountCommand.arguments(enabled: true, name: "henry@example.com", org: personal),
            ["enable", "henry@example.com", "--org", personal]
        )
        XCTAssertTrue(
            LoginLauncher.script(
                forExecutableAt: "/usr/local/bin/tcr",
                reloggingIn: "henry@example.com",
                org: personal
            ).contains("--account 'henry@example.com' --org '\(personal)'"),
            "a re-login resolves through the same refuse-on-ambiguity path"
        )
    }

    /// And the negative: a row with no org builds precisely the command it built
    /// before, with no empty flag appended. A stray `--org ''` would match
    /// nothing and break every single-org fleet.
    func testNoOrgMeansNoFlagAtAll() {
        XCTAssertEqual(
            TokenCommand.arguments(query: "solo@example.com"),
            ["token", "solo@example.com"]
        )
        XCTAssertEqual(
            RemoveAccountCommand.arguments(query: "solo@example.com"),
            ["remove", "solo@example.com"]
        )
        XCTAssertEqual(
            AccountCommand.arguments(enabled: true, name: "solo@example.com"),
            ["enable", "solo@example.com"]
        )
        let script = LoginLauncher.script(
            forExecutableAt: "/usr/local/bin/tcr", reloggingIn: "solo@example.com")
        XCTAssertTrue(script.contains("--account 'solo@example.com'"))
        XCTAssertFalse(script.contains("--org"), "no org means no flag, never an empty one")
    }

    /// An org value is quoted the same POSIX way the path and the name already
    /// are — it goes onto a command line in a `.command` file, so unquoted
    /// interpolation is injection.
    func testTheOrgArgumentIsShellQuoted() {
        let script = LoginLauncher.script(
            forExecutableAt: "/usr/local/bin/tcr",
            reloggingIn: "solo@example.com",
            org: "it's; rm -rf /"
        )
        XCTAssertTrue(script.contains("--org 'it'\\''s; rm -rf /'"))
    }

    // MARK: - fixtures

    private var duplicateEmailJSON: String {
        """
        [{"name":"henry@example.com","priority":0,"status":"active","disabled":false,
          "plan":"Max 20x","organizationType":"claude_max",
          "rateLimitTier":"default_claude_max_20x","seatTier":null,
          "orgUuid":"\(personal)","orgName":"Example Personal",
          "quota":1.0,"quotaState":"spent","fiveHour":1.0,"sevenDay":1.0,
          "sevenDayOi":null,"held":[],"requests":13011,"inputTokens":1,"outputTokens":1,
          "cacheReadTokens":1,"cacheHitRatio":0.5,"probeStatus":"ok","probeError":null,
          "lastStreamError":null,"streamErrorCount":0,"source":"live",
          "serverSha":"abc1234","serverDirty":false},
         {"name":"henry@example.com","priority":1,"status":"active","disabled":false,
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
