import Foundation

/// Which build of TcrBar is on screen.
///
/// The panel already names the build of the *proxy* it is watching
/// (`server <sha>`, in the footer) for a documented reason: the running process
/// is routinely several commits behind the source, and "the fix is in `main`"
/// and "the fix is in the thing serving traffic" are different facts. TcrBar
/// itself has the identical gap and had no answer for it. It is arguably worse
/// here — `/Applications/TcrBar.app` **cannot be replaced while it runs**
/// (`TcrTool.swift` resolves the bundled `tcr` and supervises it as a child, so
/// the bundle holds an executing image), so an update that appears to have been
/// installed can leave the old app on screen until it is quit. Every version
/// key needed to say so was already written into the bundle by
/// `scripts/build-tcrbar.sh`; nothing read them.
///
/// Every field is `nil` when absent rather than defaulted to a plausible
/// string, and ``label`` returns `nil` rather than inventing a version. The
/// only case that matters is a binary run outside its bundle
/// (`swift run TcrBar`), which has no Info.plist and therefore no version — and
/// a panel claiming `0.0.0` there would be a fabricated build number in the one
/// place a reader goes to check what they are running.
public enum AppBuild {
    /// `CFBundleShortVersionString` — the MARKETING version, which
    /// `build-tcrbar.sh` reads from `Cargo.toml`. The number that matches
    /// `tcr --version`, the git tag and the appcast entry.
    public static var shortVersion: String? { string(for: "CFBundleShortVersionString") }

    /// `CFBundleVersion` — the build number, the commit count. Not on the
    /// panel: it is what macOS and Sparkle ORDER on, not what a human calls a
    /// release, and the sha below identifies a build more usefully.
    public static var buildNumber: String? { string(for: "CFBundleVersion") }

    /// `TcrGitSHA` — the short commit, already `-dirty`-suffixed by the build
    /// script. Written into every bundle since the app shipped and read by
    /// nothing until now.
    ///
    /// `"unknown"` is what the script writes when it builds outside a git
    /// checkout; it is mapped back to `nil` here so no caller has to know that
    /// one magic string, and so it can never reach the panel as a word that
    /// looks like a commit.
    public static var gitSha: String? {
        guard let sha = string(for: "TcrGitSHA"), sha != "unknown" else { return nil }
        return sha
    }

    /// `"TcrBar 0.2.29 · 9582244"`, or `"TcrBar 0.2.29"` with no usable sha, or
    /// `nil` when there is no version to state at all.
    ///
    /// Named, not bare: it sits one row below `server <sha>`, and two
    /// unlabelled hashes in the same footer would be a puzzle rather than a
    /// fact. The separator is the `·` the rest of the panel uses.
    public static var label: String? { label(version: shortVersion, sha: gitSha) }

    /// The formatting rule on its own, so a test can exercise the absent cases
    /// without a bundle to stage them in.
    public static func label(version: String?, sha: String?) -> String? {
        guard let version else { return nil }
        guard let sha else { return "TcrBar \(version)" }
        return "TcrBar \(version) · \(sha)"
    }

    /// `"build 258"`, for the hover help — the ordering key, kept off the line
    /// itself and one hover away for whoever is comparing two installs.
    public static func buildDetail(buildNumber: String?) -> String? {
        guard let buildNumber else { return nil }
        return "build \(buildNumber)"
    }

    private static func string(for key: String) -> String? {
        guard let value = Bundle.main.object(forInfoDictionaryKey: key) as? String,
            !value.isEmpty
        else { return nil }
        return value
    }
}
