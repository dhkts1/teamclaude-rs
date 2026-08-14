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

    /// The re-login hint is honest that the browser choice — not this script —
    /// is what selects the account: `tcr login` takes no account argument.
    func testReloginHintNamesTheAccountAndDefersToTheBrowser() {
        let script = LoginLauncher.script(
            forExecutableAt: "/opt/homebrew/bin/tcr", reloggingIn: "alice@example.com")
        XCTAssertTrue(
            script.contains(
                "echo 'Re-logging in alice@example.com — choose that account in the browser.'"),
            script
        )
    }

    /// Omitting the hint must leave the existing add-account script unchanged.
    func testNoHintByDefault() {
        let script = LoginLauncher.script(forExecutableAt: "/opt/homebrew/bin/tcr")
        XCTAssertFalse(script.contains("Re-logging in"), script)
    }

    /// The injection case for the account name: a single quote must not escape
    /// the quoting and let anything after it run as its own command. Mirrors
    /// ``testSingleQuoteInPathCannotEscapeTheQuoting`` for the path.
    func testSingleQuoteInReloginNameCannotEscapeTheQuoting() {
        let script = LoginLauncher.script(
            forExecutableAt: "/opt/homebrew/bin/tcr", reloggingIn: "ev'il@example.com")
        XCTAssertTrue(
            script.contains(
                #"echo 'Re-logging in ev'\''il@example.com — choose that account in the browser.'"#),
            "a quote in the name must be escaped POSIX-style, got: \(script)"
        )
        // The whole message is one shell argument to `echo` — nothing after
        // the closing quote on that line.
        let echoLine = script.split(separator: "\n").first { $0.hasPrefix("echo 'Re-logging in") }
        XCTAssertEqual(echoLine?.hasSuffix("browser.'"), true, "trailing text after the quoted message")
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
