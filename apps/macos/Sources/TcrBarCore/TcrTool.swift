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
    /// `defaults write io.github.dhkts1.tcrbar TcrExecutablePath <path>`.
    public static let overrideDefaultsKey = "TcrExecutablePath"
    /// Environment override, useful when launched from a shell.
    public static let overrideEnvKey = "TCR_BIN"

    /// What to tell someone whose `tcr` could not be found.
    ///
    /// Stated once because it is given in two places — the poll banner and
    /// every login hand-off failure — and a remedy that drifts between them is
    /// worse than one that is missing. The hand-offs used to say only "tcr not
    /// found (searched N locations)", which names the problem and no way out
    /// of it, while the banner three hundred lines away already knew the fix.
    public static let overrideRemedy =
        "Set it with `defaults write io.github.dhkts1.tcrbar \(overrideDefaultsKey) <path>`."

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

    /// Set `SIGPIPE` to ignored, once per process.
    ///
    /// Process-wide and irreversible, which is why it is behind a
    /// `dispatch_once` rather than set and restored around the write: a
    /// restore window would be open on some other thread's write, and every
    /// other writer in this app (two more pipes per invocation) wants the same
    /// disposition anyway. Ignored means a write to a dead pipe returns
    /// `EPIPE` to the caller instead of terminating the process, which is what
    /// ``run(executable:arguments:stdin:)`` needs and what every server-side
    /// runtime does on startup for the same reason.
    ///
    /// Not `SIG_DFL`-restoring and not per-thread: `pthread_sigmask` cannot
    /// change a disposition, only a mask, and a masked `SIGPIPE` on a write
    /// stays pending rather than being discarded.
    static func ignoreSIGPIPE() {
        // Reading the value is what runs it: a `static let`'s initialiser is
        // lazy and runs exactly once, under the runtime's own lock.
        _ = sigpipeIsIgnored
    }

    /// `true` once `SIGPIPE` has been set to ignored. The value is never read
    /// for its content, the initialiser's side effect IS the point.
    private static let sigpipeIsIgnored: Bool = {
        signal(SIGPIPE, SIG_IGN)
        return true
    }()

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
    ///
    /// `stdin` is for the verbs whose input is a SECRET:
    /// ``PeerSecretInvocation`` is the only caller, and it is written and
    /// closed immediately after `run()`. Closing is not optional: a `tcr` that
    /// reads to EOF waits forever on a pipe the parent still holds open, and
    /// this function would then block in `readDataToEndOfFile` with nothing
    /// arriving. Written before the reads because these payloads are one short
    /// line: a write larger than the 64 KiB pipe buffer would have to be
    /// interleaved with the reads instead.
    ///
    /// When `stdin` is nil the child inherits this process's own stdin, which
    /// is what every other verb has always done.
    ///
    /// # Why the write cannot be `FileHandle.write(_:)`
    ///
    /// **A child that exits before reading kills this app.** The `tcr` in this
    /// tree refuses `peer join --stdin` on a spent invite, and clap refuses an
    /// unknown flag, and either one exits without reading a byte, so the pipe
    /// is closed at the far end by the time this line runs. A write to a closed
    /// pipe raises `SIGPIPE`, whose default disposition terminates the process:
    /// not the subprocess, THIS one, the menu-bar app. `FileHandle.write(_:)`
    /// cannot help, because it has no error to return and raises an
    /// `NSException` on top. So: `SIGPIPE` is ignored once for the process
    /// (``ignoreSIGPIPE()``), and the write goes through
    /// `write(contentsOf:)`, which then returns `EPIPE` as a thrown error this
    /// function swallows into the OUTPUT rather than a crash.
    ///
    /// Swallowed, and not rethrown, on purpose: a child that refused its input
    /// still has an exit code and a `stderr` saying why, and that sentence is
    /// what the operator needs. Throwing here would replace `tcr`'s own words
    /// with "broken pipe", which names the plumbing and not the problem. It is
    /// not a silent fallback either, the refusal is reported, by the process
    /// that made it.
    public static func run(
        executable: URL, arguments: [String], stdin: String? = nil
    ) throws -> Output {
        let process = Process()
        process.executableURL = executable
        process.arguments = arguments
        let out = Pipe()
        let err = Pipe()
        process.standardOutput = out
        process.standardError = err
        let input = stdin.map { _ in Pipe() }
        if let input { process.standardInput = input }
        if stdin != nil { ignoreSIGPIPE() }
        try process.run()
        if let input, let stdin {
            // `try?` on both halves: the child may be gone, and both the write
            // and the close then fail with `EPIPE`/`EBADF`. Neither is this
            // app's failure to report, the child's exit code is.
            try? input.fileHandleForWriting.write(contentsOf: Data(stdin.utf8))
            try? input.fileHandleForWriting.close()
        }
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
