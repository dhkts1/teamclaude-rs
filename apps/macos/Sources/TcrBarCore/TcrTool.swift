import Foundation

/// Locating and invoking the `tcr` binary.
///
/// The app shells out to the CLI and never speaks HTTP to the proxy: the status
/// endpoint requires the operator's proxy API key with no loopback exemption, and
/// a menu-bar app has no business holding that secret. `tcr status --json`
/// authenticates itself, so shelling out keeps this process credential-free. It
/// also never reads the tcr config file.
public enum TcrTool {
    /// User-facing override, e.g.
    /// `defaults write com.github.dhkts1.tcrbar TcrExecutablePath <path>`.
    public static let overrideDefaultsKey = "TcrExecutablePath"
    /// Environment override, useful when launched from a shell.
    public static let overrideEnvKey = "TCR_BIN"

    /// Why no binary could be found — carries the searched paths so the UI can be
    /// specific instead of silently empty.
    public struct NotFound: Error, Equatable {
        public let searched: [String]
    }

    /// Directories searched when `PATH` does not contain `tcr`.
    ///
    /// A GUI launched from Finder inherits a minimal `PATH`
    /// (`/usr/bin:/bin:/usr/sbin:/sbin`), which is why the common install
    /// locations are probed explicitly. All of them are derived — no absolute
    /// user path is hard-coded.
    public static func fallbackDirectories(
        home: URL = FileManager.default.homeDirectoryForCurrentUser
    ) -> [URL] {
        [
            home.appendingPathComponent(".local/bin", isDirectory: true),
            home.appendingPathComponent(".cargo/bin", isDirectory: true),
            URL(fileURLWithPath: "/opt/homebrew/bin", isDirectory: true),
            URL(fileURLWithPath: "/usr/local/bin", isDirectory: true),
        ]
    }

    /// The directory holding this app's own executable, which is where
    /// `build-tcrbar.sh` also puts the `tcr` it bundles (`Contents/MacOS/`).
    ///
    /// Derived from `Bundle.main`, never hard-coded: this repository is public
    /// and no user-absolute path belongs in it. `executableURL` is used rather
    /// than `bundleURL` because it resolves correctly for BOTH shapes this code
    /// runs in — inside `TcrBar.app` it is `…/TcrBar.app/Contents/MacOS/TcrBar`,
    /// and under `swift run`/`swift test` it is the bare tool — while
    /// `bundleURL` alone would need a different suffix for each.
    ///
    /// `nil` when the bundle cannot name its executable; the caller then simply
    /// has no bundle candidate to probe.
    public static func bundledDirectory(bundle: Bundle = .main) -> URL? {
        bundle.executableURL?.deletingLastPathComponent()
    }

    /// Candidate directories, `PATH` first, in probe order.
    public static func searchDirectories(
        environment: [String: String] = ProcessInfo.processInfo.environment,
        home: URL = FileManager.default.homeDirectoryForCurrentUser
    ) -> [URL] {
        let fromPath = (environment["PATH"] ?? "")
            .split(separator: ":", omittingEmptySubsequences: true)
            .map { URL(fileURLWithPath: String($0), isDirectory: true) }
        var seen = Set<String>()
        return (fromPath + fallbackDirectories(home: home)).filter { seen.insert($0.path).inserted }
    }

    /// Resolve the binary, honouring the env override, then the defaults
    /// override, then the bundled binary, then the search path.
    ///
    /// The bundled binary sits between the two explicit overrides and `PATH` on
    /// purpose. An operator who names a path in `TCR_BIN` or the defaults key
    /// means it, so those still win. But a `tcr` that shipped inside this very
    /// bundle must beat whatever happens to be on `PATH`: the app and the server
    /// are built and installed as one artifact precisely so they cannot drift,
    /// and letting an older `PATH` copy win would give that away for nothing.
    public static func resolve(
        environment: [String: String] = ProcessInfo.processInfo.environment,
        defaults: UserDefaults = .standard,
        home: URL = FileManager.default.homeDirectoryForCurrentUser,
        fileManager: FileManager = .default,
        bundle: URL? = bundledDirectory()
    ) -> Result<URL, NotFound> {
        var searched: [String] = []
        for override in [environment[overrideEnvKey], defaults.string(forKey: overrideDefaultsKey)] {
            guard let override, !override.isEmpty else { continue }
            let url = URL(fileURLWithPath: override)
            searched.append(url.path)
            if fileManager.isExecutableFile(atPath: url.path) { return .success(url) }
        }
        if let bundle {
            let candidate = bundle.appendingPathComponent("tcr")
            searched.append(candidate.path)
            if fileManager.isExecutableFile(atPath: candidate.path) { return .success(candidate) }
        }
        for dir in searchDirectories(environment: environment, home: home) {
            let candidate = dir.appendingPathComponent("tcr")
            searched.append(candidate.path)
            if fileManager.isExecutableFile(atPath: candidate.path) { return .success(candidate) }
        }
        return .failure(NotFound(searched: searched))
    }

    /// A finished invocation.
    public struct Output: Equatable {
        public let exitCode: Int32
        public let stdout: Data
        public let stderr: String

        public init(exitCode: Int32, stdout: Data, stderr: String) {
            self.exitCode = exitCode
            self.stdout = stdout
            self.stderr = stderr
        }
    }

    /// Run `tcr` to completion and collect both streams.
    ///
    /// Reads happen before `waitUntilExit()` because a pipe that fills while the
    /// parent is blocked in `wait` deadlocks the child.
    public static func run(executable: URL, arguments: [String]) throws -> Output {
        let process = Process()
        process.executableURL = executable
        process.arguments = arguments
        let out = Pipe()
        let err = Pipe()
        process.standardOutput = out
        process.standardError = err
        try process.run()
        let stdout = out.fileHandleForReading.readDataToEndOfFile()
        let stderr = err.fileHandleForReading.readDataToEndOfFile()
        process.waitUntilExit()
        return Output(
            exitCode: process.terminationStatus,
            stdout: stdout,
            stderr: String(data: stderr, encoding: .utf8) ?? ""
        )
    }
}
