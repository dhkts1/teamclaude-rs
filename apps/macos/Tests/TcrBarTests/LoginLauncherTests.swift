import XCTest

@testable import TcrBarCore

/// The script handed to Terminal is a composed shell command, which is the one
/// place in this app where a path becomes executable text. These tests exist for
/// the quoting.
final class LoginLauncherTests: XCTestCase {

    func testOrdinaryPathIsQuotedAndRunsLogin() {
        let script = LoginLauncher.script(forExecutableAt: "/opt/homebrew/bin/tcr")
        XCTAssertTrue(script.contains("exec '/opt/homebrew/bin/tcr' login"), script)
    }

    /// An install path with a space must stay ONE argument. Unquoted, the shell
    /// would try to run `/Applications/My` with `Tools/tcr` as an argument.
    func testPathWithSpacesStaysASingleArgument() {
        let script = LoginLauncher.script(forExecutableAt: "/Users/x/My Tools/tcr")
        XCTAssertTrue(script.contains("exec '/Users/x/My Tools/tcr' login"), script)
    }

    /// The injection case. A single quote inside the path would otherwise close
    /// the quoting and let everything after it run as its own command.
    func testSingleQuoteInPathCannotEscapeTheQuoting() {
        let script = LoginLauncher.script(forExecutableAt: "/tmp/ev'il/tcr")

        XCTAssertTrue(
            script.contains(#"exec '/tmp/ev'\''il/tcr' login"#),
            "a quote must be escaped POSIX-style, got: \(script)"
        )
        // Nothing may follow the quoted path except the subcommand.
        let execLine = script.split(separator: "\n").first { $0.hasPrefix("exec ") }
        XCTAssertEqual(execLine?.hasSuffix("' login"), true, "trailing text after the path")
    }

    /// `--force` exists in `tcr login` and is documented there as unsafe: it logs
    /// in past the running-server guard, and the server's next token refresh then
    /// overwrites the login. A GUI must never pass it silently.
    func testNeverPassesForce() {
        for path in ["/usr/local/bin/tcr", "/Users/x/My Tools/tcr", "/tmp/ev'il/tcr"] {
            XCTAssertFalse(
                LoginLauncher.script(forExecutableAt: path).contains("--force"),
                "script for \(path) must not force past the login guard"
            )
        }
    }

    /// A missing tool is reported, not silently swallowed into a button that does
    /// nothing when clicked.
    func testMissingToolIsReportedWithWhatWasSearched() {
        var opened: URL?
        let result = LoginLauncher.launch(
            resolve: { .failure(TcrTool.NotFound(searched: ["/a/tcr", "/b/tcr"])) },
            open: { opened = $0 }
        )

        guard case .failure(.toolMissing(let searched)) = result else {
            return XCTFail("expected a toolMissing failure, got \(result)")
        }
        XCTAssertEqual(searched, ["/a/tcr", "/b/tcr"])
        XCTAssertNil(opened, "nothing should be opened when tcr was never found")
    }

    /// The re-login hint is honest about what actually targets the account:
    /// `--account` requests that identity and `tcr` refuses to save on a
    /// mismatch — not "choose that account in the browser", which was true
    /// before `--account` shipped and is not any more.
    func testReloginHintNamesTheAccountAndTheMismatchGuard() {
        let script = LoginLauncher.script(
            forExecutableAt: "/opt/homebrew/bin/tcr", reloggingIn: "alice@example.com")
        XCTAssertTrue(
            script.contains(
                "echo 'Re-logging in alice@example.com — tcr requests that account, "
                    + "and refuses to save if the browser hands back a different one.'"
            ),
            script
        )
    }

    /// The part that makes the button actually target the row it was clicked
    /// from, not merely narrate it: `login --account <name>` on the exec line.
    /// `src/main.rs` / `src/oauth.rs`'s `login_hint` refuses to write when the
    /// browser hands back a different identity — measured live, a re-login
    /// meant for one account authenticated as a different one signed into the
    /// browser, and this flag is the only reason nothing was overwritten.
    func testReloginPassesAccountFlagOnTheExecLine() {
        let script = LoginLauncher.script(
            forExecutableAt: "/opt/homebrew/bin/tcr", reloggingIn: "alice@example.com")
        XCTAssertTrue(
            script.contains("exec '/opt/homebrew/bin/tcr' login --account 'alice@example.com'"),
            script
        )
    }

    /// Omitting the hint must leave the existing add-account script unchanged
    /// — no echoed name, and no `--account` flag it cannot satisfy.
    func testNoHintOrAccountFlagByDefault() {
        let script = LoginLauncher.script(forExecutableAt: "/opt/homebrew/bin/tcr")
        XCTAssertFalse(script.contains("Re-logging in"), script)
        XCTAssertFalse(script.contains("--account"), script)
        // The exec line ends at `login`, whole-line, not merely a substring —
        // an `--account` this build failed to append would otherwise slip
        // past a plain `.contains("login")` check.
        let execLine = script.split(separator: "\n").first { $0.hasPrefix("exec ") }
        XCTAssertEqual(
            execLine.map(String.init), "exec '/opt/homebrew/bin/tcr' login",
            "the add-account path must not grow an --account it cannot satisfy"
        )
    }

    /// The injection case for the account name in the ECHO: a single quote
    /// must not escape the quoting and let anything after it run as its own
    /// command. Mirrors ``testSingleQuoteInPathCannotEscapeTheQuoting`` for
    /// the path.
    func testSingleQuoteInReloginNameCannotEscapeTheEchoQuoting() {
        let script = LoginLauncher.script(
            forExecutableAt: "/opt/homebrew/bin/tcr", reloggingIn: "ev'il@example.com")
        XCTAssertTrue(
            script.contains(
                #"echo 'Re-logging in ev'\''il@example.com — tcr requests that account, "#
                    + #"and refuses to save if the browser hands back a different one.'"#
            ),
            "a quote in the name must be escaped POSIX-style, got: \(script)"
        )
        // The whole message is one shell argument to `echo` — nothing after
        // the closing quote on that line.
        let echoLine = script.split(separator: "\n").first { $0.hasPrefix("echo 'Re-logging in") }
        XCTAssertEqual(echoLine?.hasSuffix("different one.'"), true, "trailing text after the quoted message")
    }

    /// The injection case for the account name on the COMMAND LINE — the part
    /// that is now load-bearing rather than cosmetic, since this text becomes
    /// an actual shell argument to `tcr`, not just something `echo` prints.
    func testSingleQuoteInReloginNameCannotEscapeTheAccountFlagQuoting() {
        let script = LoginLauncher.script(
            forExecutableAt: "/opt/homebrew/bin/tcr", reloggingIn: "ev'il@example.com")
        XCTAssertTrue(
            script.contains(#"login --account 'ev'\''il@example.com'"#),
            "a quote in the name must be escaped POSIX-style on the exec line, got: \(script)"
        )
        // Nothing may follow the quoted account name except the end of the line.
        let execLine = script.split(separator: "\n").first { $0.hasPrefix("exec ") }
        XCTAssertEqual(execLine?.hasSuffix("'"), true, "trailing text after the quoted account name")
    }

    /// The race this fixes: before `--account`, every invocation wrote
    /// identical bytes to a FIXED path, so two overlapping Re-login clicks
    /// were harmless. The content is per-account now, so two clicks in quick
    /// succession could overwrite the file before the first Terminal window
    /// reads it — window A running window B's `--account`. Two real (default
    /// `UUID.init`) launches must land at two different paths.
    func testTwoLaunchesGetDifferentPaths() throws {
        var opened: [URL] = []
        for _ in 0..<2 {
            let result = LoginLauncher.launch(
                resolve: { .success(URL(fileURLWithPath: "/usr/local/bin/tcr")) },
                open: { opened.append($0) }
            )
            guard case .success = result else { return XCTFail("expected success") }
        }
        XCTAssertEqual(opened.count, 2)
        XCTAssertNotEqual(opened[0], opened[1], "two launches must not race on one shared path")
    }

    /// The path is deterministic under an injected UUID, which is what makes
    /// the uniqueness above testable without depending on real randomness.
    func testPathIncorporatesTheInjectedUUID() throws {
        let fixed = UUID(uuidString: "11111111-1111-1111-1111-111111111111")!
        var opened: URL?
        let result = LoginLauncher.launch(
            uuid: { fixed },
            resolve: { .success(URL(fileURLWithPath: "/usr/local/bin/tcr")) },
            open: { opened = $0 }
        )
        guard case .success(let url) = result else { return XCTFail("expected success") }
        XCTAssertEqual(opened, url)
        XCTAssertEqual(url.lastPathComponent, "tcr-login-\(fixed.uuidString).command")
    }

    func testSuccessWritesAnExecutableScriptAndOpensIt() throws {
        var opened: URL?
        let result = LoginLauncher.launch(
            resolve: { .success(URL(fileURLWithPath: "/usr/local/bin/tcr")) },
            open: { opened = $0 }
        )

        guard case .success(let url) = result else {
            return XCTFail("expected success, got \(result)")
        }
        XCTAssertEqual(opened, url)
        XCTAssertEqual(url.pathExtension, "command", "Terminal opens .command files")

        let written = try String(contentsOf: url, encoding: .utf8)
        XCTAssertTrue(written.contains("exec '/usr/local/bin/tcr' login"), written)

        let mode = try FileManager.default.attributesOfItem(atPath: url.path)[.posixPermissions]
        XCTAssertEqual(mode as? NSNumber, 0o700, "must be executable, and only by its owner")
    }
}
