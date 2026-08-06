import AppKit
import Foundation

/// Hands `tcr login` to a real Terminal window.
///
/// ## Why not just spawn it
///
/// `tcr login` cannot run as a background subprocess of a GUI app, for two
/// independent reasons, and both are in `tcr`'s own source rather than guesswork:
///
///  1. **It refuses while a server holds the port** (`src/oauth.rs:752-757`):
///     logging in then would be overwritten by the server's next token refresh.
///     TcrBar exists because a proxy is always running, so that refusal is the
///     normal case here, not an edge case. Its message names the pid and the
///     stop-login-restart sequence — which is useful only if a human can read it.
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
    public static func script(forExecutableAt path: String) -> String {
        // Single-quote the path and escape any embedded single quote the POSIX
        // way ('\'') so the shell receives exactly one argument whatever the path
        // contains.
        let quoted = "'" + path.replacingOccurrences(of: "'", with: "'\\''") + "'"
        return """
            #!/bin/sh
            # Opened by TcrBar. `tcr login` needs a terminal: it prompts for an
            # account name and may ask you to paste an authorization code, and it
            # refuses outright while a proxy is holding the port.
            echo "Running tcr login — follow the prompts below."
            echo
            exec \(quoted) login
            """
    }

    /// Write the script somewhere Terminal will open, and open it.
    ///
    /// A `.command` file opened via LaunchServices starts Terminal directly. The
    /// alternative — an AppleScript `do script` — needs Automation permission and
    /// would put a consent dialog between the operator and a login they asked for.
    @discardableResult
    public static func launch(
        resolve: () -> Result<URL, TcrTool.NotFound> = { TcrTool.resolve() },
        open: (URL) -> Void = { NSWorkspace.shared.open($0) }
    ) -> Result<URL, Failure> {
        let executable: URL
        switch resolve() {
        case .success(let url): executable = url
        case .failure(let missing): return .failure(.toolMissing(searched: missing.searched))
        }

        let script = script(forExecutableAt: executable.path)
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
