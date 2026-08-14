import AppKit
import Foundation

/// Hands `tcr login` to a real Terminal window.
///
/// ## Why not just spawn it
///
/// `tcr login` cannot run as a background subprocess of a GUI app, for two
/// independent reasons, and both are in `tcr`'s own source rather than guesswork:
///
///  1. **An older `tcr` refuses while a server holds the port.** That used to be
///     universal (`src/oauth.rs:752-757`, superseded), but `a385f0f`
///     (2026-08-11, "feat: route tcr login through a live proxy instead of
///     refusing") added a live route: `login_route` (`src/oauth.rs:953-997`)
///     probes the running proxy and, on `AddCapability::Present`, takes the
///     login live through the server instead of refusing. A modern proxy — the
///     only kind this button needs to assume — accepts a login while serving.
///     An old one still refuses, before any browser opens, and its message
///     names the pid and the stop-login-restart sequence — which is useful
///     only if a human can read it.
///  2. **It is interactive.** It prompts for an account name on stdin
///     (`src/oauth.rs:645`) and can take a pasted authorization code
///     (`src/oauth.rs:450`). With no TTY those prompts go nowhere and stdin hits
///     EOF immediately.
///
/// So the button hands off rather than pretending. In a Terminal window the
/// prompts are visible, the browser callback still works, and tcr's refusal — if
/// it refuses — is read by the person who can act on it.
///
/// `--force` is never passed. `tcr` documents it as unsafe, and a GUI silently
/// forcing past a guard the CLI put up is exactly the wrong use of a button.
public enum LoginLauncher {

    public enum Failure: Error, Equatable {
        case toolMissing(searched: [String])
        case couldNotWriteScript(String)
    }

    /// Compose the shell script that a Terminal window will run.
    ///
    /// Split out and pure so the quoting is testable: an install path containing a
    /// space or a quote must not become a broken or, worse, an injectable command.
    ///
    /// `reloggingIn` is an optional account-name hint, `nil` by default so the
    /// existing add-account call site is unchanged. When present, the script
    /// echoes which account this re-login is for — honestly: `tcr login` takes
    /// no account argument, it upserts by the profile identity the browser hands
    /// back, so the label says the browser choice is what actually selects the
    /// account, rather than implying this script can steer it there.
    ///
    /// Shell-quoted exactly like the path just below — POSIX `'\''` — because an
    /// account name is attacker-adjacent input in principle, and unquoted
    /// interpolation into a `.command` file is injection.
    public static func script(forExecutableAt path: String, reloggingIn name: String? = nil) -> String {
        // Single-quote the path and escape any embedded single quote the POSIX
        // way ('\'') so the shell receives exactly one argument whatever the path
        // contains.
        let quoted = "'" + path.replacingOccurrences(of: "'", with: "'\\''") + "'"
        let hint: String
        if let name {
            // The WHOLE message is the single-quoted argument, not the name
            // alone inside a double-quoted one — a double-quoted echo still
            // expands `$`, backticks and `\`, so quoting only the name would
            // leave the rest of the line open to exactly the injection this
            // quoting exists to close.
            let message = "Re-logging in \(name) — choose that account in the browser."
            let quotedMessage = "'" + message.replacingOccurrences(of: "'", with: "'\\''") + "'"
            hint = """
                echo \(quotedMessage)
                echo

                """
        } else {
            hint = ""
        }
        return """
            #!/bin/sh
            # Opened by TcrBar. `tcr login` needs a terminal: it prompts for an
            # account name and may ask you to paste an authorization code. A
            # modern proxy takes the login live even while serving; an older one
            # refuses outright while a proxy is holding the port.
            echo "Running tcr login — follow the prompts below."
            echo
            \(hint)exec \(quoted) login
            """
    }

    /// Write the script somewhere Terminal will open, and open it.
    ///
    /// A `.command` file opened via LaunchServices starts Terminal directly. The
    /// alternative — an AppleScript `do script` — needs Automation permission and
    /// would put a consent dialog between the operator and a login they asked for.
    @discardableResult
    public static func launch(
        reloggingIn name: String? = nil,
        resolve: () -> Result<URL, TcrTool.NotFound> = { TcrTool.resolve() },
        open: (URL) -> Void = { NSWorkspace.shared.open($0) }
    ) -> Result<URL, Failure> {
        let executable: URL
        switch resolve() {
        case .success(let url): executable = url
        case .failure(let missing): return .failure(.toolMissing(searched: missing.searched))
        }

        let script = script(forExecutableAt: executable.path, reloggingIn: name)
        let url = FileManager.default.temporaryDirectory
            .appendingPathComponent("tcr-login.command")

        do {
            try script.write(to: url, atomically: true, encoding: .utf8)
            try FileManager.default.setAttributes(
                [.posixPermissions: 0o700], ofItemAtPath: url.path)
        } catch {
            return .failure(.couldNotWriteScript(error.localizedDescription))
        }

        open(url)
        return .success(url)
    }
}
