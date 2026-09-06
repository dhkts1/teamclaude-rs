import Foundation

/// Reading one account's access token for the row's "Copy Access Token"
/// action — `tcr token <query> [--org <org>]` (`src/cli.rs`'s
/// `print_access_token`). A subprocess, like every other credential touch in
/// this app: `~/.config/teamclaude.json` holds live OAuth tokens and this app
/// never opens it directly.
///
/// The token is the whole of stdout, one line. It is a secret: the caller
/// puts it on the pasteboard and nowhere else — never in a log, a label, or
/// a failure message. Failures carry only `tcr`'s stderr, which names the
/// account, not the credential.
public enum TokenCommand {
    /// `query` is passed positionally and verbatim — no shell involved.
    public static func arguments(query: String, org: String? = nil) -> [String] {
        guard let org else { return ["token", query] }
        return ["token", query, "--org", org]
    }

    /// Why no token was produced. `tcr`'s own words, unparaphrased.
    public struct Failure: Error, Equatable, Sendable {
        public let exitCode: Int32
        public let message: String

        public init(exitCode: Int32, message: String) {
            self.exitCode = exitCode
            self.message = message
        }

        /// One line, always non-empty, safe to render in the row.
        public var summary: String {
            let detail = message.isEmpty ? "no output" : message
            return "copy token failed (exit \(exitCode)): \(detail)"
        }
    }

    /// Pure classification of a finished invocation. Exit 0 with an empty
    /// stdout is a failure, not a success: an empty pasteboard would look
    /// exactly like a copy that landed.
    public static func classify(exitCode: Int32, stdout: Data, stderr: String) -> Result<String, Failure> {
        let text = stderr.trimmingCharacters(in: .whitespacesAndNewlines)
        guard exitCode == 0 else {
            return .failure(Failure(exitCode: exitCode, message: text))
        }
        let token = (String(data: stdout, encoding: .utf8) ?? "")
            .trimmingCharacters(in: .whitespacesAndNewlines)
        guard !token.isEmpty else {
            return .failure(Failure(exitCode: exitCode, message: "tcr printed no token"))
        }
        return .success(token)
    }

    /// Blocking invocation — always called off the main actor.
    nonisolated static func perform(query: String, org: String? = nil) -> Result<String, Failure> {
        switch TcrTool.resolve() {
        case .failure(let notFound):
            return .failure(
                Failure(
                    exitCode: -1,
                    message: "tcr not found (searched \(notFound.searched.count) locations)"
                ))
        case .success(let executable):
            do {
                let output = try TcrTool.run(
                    executable: executable, arguments: arguments(query: query, org: org))
                return classify(exitCode: output.exitCode, stdout: output.stdout, stderr: output.stderr)
            } catch {
                return .failure(Failure(exitCode: -1, message: error.localizedDescription))
            }
        }
    }

    /// Run `tcr token` off the main actor and hand back the token, or why not.
    public static func fetch(query: String, org: String? = nil) async -> Result<String, Failure> {
        await Task.detached(priority: .userInitiated) {
            perform(query: query, org: org)
        }.value
    }
}
