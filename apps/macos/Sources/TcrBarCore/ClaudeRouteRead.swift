import Foundation

/// Where Claude Code will send its traffic: the base URL it will use, and
/// where that comes from. This is the one answer this app can give on its
/// own, kept self-contained so the Accounts tab's no-requests banner does
/// not have to wait on a fuller diagnostic that reads more sources.
///
/// Deliberately narrower than a full resolution order could be: it does not
/// read process env (this app's own environment is not the `claude`
/// process's, `tcr run` sets `ANTHROPIC_BASE_URL` on ITS OWN child,
/// `src/main.rs:1183`, which is never TcrBar) and it does not read the
/// project-level `.claude/settings.json` inside a session's cwd (this app
/// has no cwd to read one from). This type answers the one question it can
/// answer honestly on its own: the user-level settings file, or the
/// default.
public enum ClaudeRouteRead {
    public static let defaultBaseURL = "https://api.anthropic.com"

    public struct Route: Equatable {
        public let url: String
        public let source: String

        public init(url: String, source: String) {
            self.url = url
            self.source = source
        }
    }

    /// `home` is injectable so a test points at a temp directory instead of
    /// the real one. Never written here as a literal `~`, which `FileManager`
    /// does not expand and this repo's own added-lines rule forbids anyway.
    public static func current(
        home: URL = FileManager.default.homeDirectoryForCurrentUser
    ) -> Route {
        let settingsPath =
            home
            .appendingPathComponent(".claude", isDirectory: true)
            .appendingPathComponent("settings.json")
        if let url = baseURL(inSettingsAt: settingsPath) {
            return Route(url: url, source: "settings.json")
        }
        return Route(url: defaultBaseURL, source: "default")
    }

    static func baseURL(inSettingsAt path: URL) -> String? {
        guard let data = try? Data(contentsOf: path),
            let object = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
            let env = object["env"] as? [String: Any],
            let url = env["ANTHROPIC_BASE_URL"] as? String,
            !url.isEmpty
        else { return nil }
        return url
    }
}
