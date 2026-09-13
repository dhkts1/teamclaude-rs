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

    // MARK: - `tcr mint`

    /// `tcr mint --account <name>` on the exec line for the account target.
    func testMintAccountScriptRunsMintAccountFlag() {
        let script = LoginLauncher.mintScript(
            forExecutableAt: "/opt/homebrew/bin/tcr", target: .account("alice"))
        XCTAssertTrue(
            script.contains("exec '/opt/homebrew/bin/tcr' mint --account 'alice'"), script)
    }

    /// `tcr mint --group <name>` on the exec line for the group target.
    func testMintGroupScriptRunsMintGroupFlag() {
        let script = LoginLauncher.mintScript(
            forExecutableAt: "/opt/homebrew/bin/tcr", target: .group("dev"))
        XCTAssertTrue(
            script.contains("exec '/opt/homebrew/bin/tcr' mint --group 'dev'"), script)
    }

    /// The injection case for an account name containing both a space and a
    /// single quote — the brief's own minimum bar. Mirrors
    /// ``testSingleQuoteInPathCannotEscapeTheQuoting`` for the login script:
    /// the whole name must stay one shell argument.
    func testMintAccountNameWithSpaceAndQuoteIsQuotedAsOneArgument() {
        let script = LoginLauncher.mintScript(
            forExecutableAt: "/opt/homebrew/bin/tcr", target: .account("ev'il name"))
        XCTAssertTrue(
            script.contains(#"exec '/opt/homebrew/bin/tcr' mint --account 'ev'\''il name'"#),
            "a quote and a space in the account name must be escaped POSIX-style, got: \(script)"
        )
        // Nothing may follow the quoted name except the end of the line.
        let execLine = script.split(separator: "\n").first { $0.hasPrefix("exec ") }
        XCTAssertEqual(execLine?.hasSuffix("'"), true, "trailing text after the quoted account name")
    }

    /// Same injection case for a group name.
    func testMintGroupNameWithSpaceAndQuoteIsQuotedAsOneArgument() {
        let script = LoginLauncher.mintScript(
            forExecutableAt: "/opt/homebrew/bin/tcr", target: .group("ev'il group"))
        XCTAssertTrue(
            script.contains(#"exec '/opt/homebrew/bin/tcr' mint --group 'ev'\''il group'"#),
            "a quote and a space in the group name must be escaped POSIX-style, got: \(script)"
        )
        let execLine = script.split(separator: "\n").first { $0.hasPrefix("exec ") }
        XCTAssertEqual(execLine?.hasSuffix("'"), true, "trailing text after the quoted group name")
    }

    /// A missing tool is reported the same way ``launch`` reports it, not
    /// silently swallowed.
    func testLaunchMintMissingToolIsReportedWithWhatWasSearched() {
        var opened: URL?
        let result = LoginLauncher.launchMint(
            target: .account("alice"),
            resolve: { .failure(TcrTool.NotFound(searched: ["/a/tcr", "/b/tcr"])) },
            open: { opened = $0 }
        )

        guard case .failure(.toolMissing(let searched)) = result else {
            return XCTFail("expected a toolMissing failure, got \(result)")
        }
        XCTAssertEqual(searched, ["/a/tcr", "/b/tcr"])
        XCTAssertNil(opened, "nothing should be opened when tcr was never found")
    }

    /// Mirrors ``testSuccessWritesAnExecutableScriptAndOpensIt``: a real
    /// invocation writes an executable `.command` file and opens it.
    func testLaunchMintSuccessWritesAnExecutableScriptAndOpensIt() throws {
        var opened: URL?
        let result = LoginLauncher.launchMint(
            target: .group("dev"),
            resolve: { .success(URL(fileURLWithPath: "/usr/local/bin/tcr")) },
            open: { opened = $0 }
        )

        guard case .success(let url) = result else {
            return XCTFail("expected success, got \(result)")
        }
        XCTAssertEqual(opened, url)
        XCTAssertEqual(url.pathExtension, "command", "Terminal opens .command files")

        let written = try String(contentsOf: url, encoding: .utf8)
        XCTAssertTrue(written.contains("exec '/usr/local/bin/tcr' mint --group 'dev'"), written)

        let mode = try FileManager.default.attributesOfItem(atPath: url.path)[.posixPermissions]
        XCTAssertEqual(mode as? NSNumber, 0o700, "must be executable, and only by its owner")
    }

    /// Two mint launches must not race on one shared path, same reasoning as
    /// ``testTwoLaunchesGetDifferentPaths`` for login.
    func testTwoMintLaunchesGetDifferentPaths() throws {
        var opened: [URL] = []
        for _ in 0..<2 {
            let result = LoginLauncher.launchMint(
                target: .account("alice"),
                resolve: { .success(URL(fileURLWithPath: "/usr/local/bin/tcr")) },
                open: { opened.append($0) }
            )
            guard case .success = result else { return XCTFail("expected success") }
        }
        XCTAssertEqual(opened.count, 2)
        XCTAssertNotEqual(opened[0], opened[1], "two launches must not race on one shared path")
    }

    // MARK: - The in-app login (`LoginSession`, `tcr login --non-interactive`)

    /// The argv, which is the whole contract with the CLI. `--force` is absent
    /// here for the same reason it is absent from the Terminal script.
    func testInAppLoginArgumentsAndNeverForce() {
        XCTAssertEqual(
            LoginSession.arguments(account: nil), ["login", "--non-interactive"])
        XCTAssertEqual(
            LoginSession.arguments(account: "alice@example.com"),
            ["login", "--non-interactive", "--account", "alice@example.com"])
        XCTAssertFalse(LoginSession.arguments(account: "alice@example.com").contains("--force"))
    }

    /// Every line `src/oauth.rs`'s `LoginEvent::line` can print, parsed back.
    func testEveryEventLineParses() {
        XCTAssertEqual(
            LoginProgressEvent.parse(#"{"event":"browser","url":"https://claude.ai/oauth/authorize?x=1"}"#),
            .browser(url: "https://claude.ai/oauth/authorize?x=1"))
        XCTAssertEqual(LoginProgressEvent.parse(#"{"event":"waiting"}"#), .waiting)
        XCTAssertEqual(
            LoginProgressEvent.parse(#"{"event":"saved","account":"alice@example.com"}"#),
            .saved(account: "alice@example.com"))
        XCTAssertEqual(
            LoginProgressEvent.parse(#"{"event":"error","reason":"Login timed out after 2 minutes"}"#),
            .failed(reason: "Login timed out after 2 minutes"))
    }

    /// A line that is not one of the four events must be ignored, never
    /// mistaken for one. Prose on stdout is what a CLI one version ahead or
    /// behind this app looks like, and reading it as a state change is how a
    /// working login reports itself broken.
    func testNonEventLinesAreIgnored() {
        for line in [
            "",
            "   ",
            "Saved account 'alice@example.com' to /tmp/config.json",
            #"{"event":"browser"}"#,  // no url
            #"{"event":"saved","account":""}"#,  // empty name
            #"{"event":"something-new","detail":"from a newer tcr"}"#,
            #"{"not":"an event"}"#,
            "{ this is not json",
        ] {
            XCTAssertNil(LoginProgressEvent.parse(line), "must ignore: \(line)")
        }
    }

    /// The happy sequence, folded through the state machine: the `browser`
    /// event hands back a URL to open and changes nothing, `waiting` names who
    /// is signing in, `saved` ends it.
    func testHappySequenceReachesSaved() {
        var flow = LoginFlow(requesting: "alice@example.com")
        XCTAssertEqual(flow.phase, .opening)

        let url = flow.apply(.browser(url: "https://claude.ai/oauth/authorize?state=abc"))
        XCTAssertEqual(url?.absoluteString, "https://claude.ai/oauth/authorize?state=abc")
        XCTAssertEqual(flow.phase, .opening, "the URL is out, but nothing is waiting on it yet")

        XCTAssertNil(flow.apply(.waiting))
        XCTAssertEqual(flow.phase, .waitingForBrowser(email: "alice@example.com"))

        XCTAssertNil(flow.apply(.saved(account: "alice@example.com")))
        XCTAssertEqual(flow.phase, .saved(account: "alice@example.com"))
        XCTAssertTrue(flow.phase.isTerminal)
    }

    /// A fresh add has no identity to name yet — the sheet must not invent one.
    func testAFreshAddWaitsWithNoEmail() {
        var flow = LoginFlow(requesting: nil)
        flow.apply(.waiting)
        XCTAssertEqual(flow.phase, .waitingForBrowser(email: nil))
    }

    /// The error event carries `tcr`'s own words through unparaphrased.
    func testErrorEventBecomesTheFailureReason() {
        var flow = LoginFlow(requesting: nil)
        flow.apply(.waiting)
        flow.apply(.failed(reason: "the proxy on :3456 rejected the api-key"))
        XCTAssertEqual(flow.phase, .failed(reason: "the proxy on :3456 rejected the api-key"))
    }

    /// A child that dies saying nothing must still end the sheet. Without this
    /// the spinner runs forever on a process that is gone — which is exactly
    /// what an OLD `tcr` does, exiting immediately on an unknown argument.
    func testAChildThatExitsSilentlyStillFails() {
        var flow = LoginFlow(requesting: nil)
        flow.apply(.waiting)
        flow.finish(exitCode: 2, stderr: "")
        XCTAssertEqual(
            flow.phase, .failed(reason: "tcr login exited (code 2) without saving an account"))
    }

    /// When it did speak, its last stderr line IS the reason: `--non-interactive`
    /// puts the whole failure on one line there.
    func testAChildThatSpokeOnStderrReportsThatReason() {
        var flow = LoginFlow(requesting: nil)
        flow.apply(.waiting)
        flow.finish(exitCode: 1, stderr: "OAuth login failed: Login timed out after 2 minutes\n")
        XCTAssertEqual(
            flow.phase, .failed(reason: "OAuth login failed: Login timed out after 2 minutes"))
    }

    /// A finished login cannot be un-finished by anything that arrives after
    /// it — including the child's own exit.
    func testNothingOverwritesATerminalPhase() {
        var flow = LoginFlow(requesting: nil)
        flow.apply(.saved(account: "alice@example.com"))
        flow.apply(.failed(reason: "a late error"))
        flow.finish(exitCode: 1, stderr: "and a late stderr line")
        XCTAssertEqual(flow.phase, .saved(account: "alice@example.com"))
    }

    /// A URL this app cannot open is a visible failure, not a skipped step:
    /// the browser is the only human part of the flow.
    func testAnUnopenableAuthorizeURLFails() {
        var flow = LoginFlow(requesting: nil)
        XCTAssertNil(flow.apply(.browser(url: "file:///etc/passwd")))
        guard case .failed(let reason) = flow.phase else {
            return XCTFail("expected a failure, got \(flow.phase)")
        }
        XCTAssertTrue(reason.contains("file:///etc/passwd"), reason)
    }

    /// The authorize URL is the longest line this stream carries and the one a
    /// pipe read is likeliest to cut in half. Split mid-URL, it must still
    /// arrive as ONE line.
    func testALineSplitAcrossTwoReadsIsStillOneLine() {
        let line = #"{"event":"browser","url":"https://claude.ai/oauth/authorize?state=abcdef"}"#
        var buffer = LineBuffer()
        let whole = Array((line + "\n").utf8)
        let cut = whole.count / 2

        XCTAssertEqual(buffer.append(Data(whole[..<cut])), [], "no complete line yet")
        XCTAssertEqual(buffer.append(Data(whole[cut...])), [line])
        XCTAssertEqual(
            LoginProgressEvent.parse(line),
            .browser(url: "https://claude.ai/oauth/authorize?state=abcdef"))
    }

    /// Two events in one read are two lines, and a trailing partial is held.
    func testTwoLinesInOneReadAndATrailingPartial() {
        var buffer = LineBuffer()
        let lines = buffer.append(Data((#"{"event":"waiting"}"# + "\n" + #"{"event":"sav"#).utf8))
        XCTAssertEqual(lines, [#"{"event":"waiting"}"#])
        XCTAssertEqual(
            buffer.append(Data((#"ed","account":"alice@example.com"}"# + "\n").utf8)),
            [#"{"event":"saved","account":"alice@example.com"}"#])
    }

    /// The capability probe reads `tcr login --help`. An older `tcr` — the one
    /// this app must hand to a Terminal window instead — does not name the flag.
    func testCapabilityProbeReadsTheHelpText() {
        XCTAssertTrue(
            LoginCapability.supportsNonInteractive(
                help: "Usage: tcr login [OPTIONS]\n      --non-interactive  Drive the login\n"))
        XCTAssertFalse(
            LoginCapability.supportsNonInteractive(
                help: "Usage: tcr login [OPTIONS]\n      --force  Skip the refusal\n"))
    }
}
