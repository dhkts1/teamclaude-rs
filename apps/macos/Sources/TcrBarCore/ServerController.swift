import Combine
import Foundation

/// Supervises a `tcr server` child process.
///
/// The safety property this type exists to hold:
///
/// `tcr`'s port singleton resolves the port down to exactly one proxy, because
/// two proxies mutually invalidate each other's single-use OAuth refresh tokens.
/// It can resolve it either way — replace the incumbent, or stand down and exit
/// without binding — and replacing is by far the more expensive: that kill wipes
/// the session→account pin map, and Anthropic's prompt cache is per-account, so
/// every live session pays a full cold prefix. It is the most expensive event in
/// this system.
///
/// So `tcr`'s default is to stand down, and the kill lives behind an explicit
/// `--replace` (`src/singleton.rs`). This controller signals **only** a child it
/// spawned itself. An incumbent server it merely observed is never touched, and
/// failing to start because one already holds the port is reported as "already
/// running" — a success, not an error.
///
/// There is one deliberate exception, `startTakingOverPort()`, and it is still
/// not a signal: it spawns `tcr server` *with* `--replace` and lets `tcr`'s own
/// singleton do the replacing, inside the code written to do it safely. This app
/// never sends a signal to a pid it did not spawn, and there is deliberately no
/// code path here that could.
@MainActor
public final class ServerController: ObservableObject {
    public enum State: Equatable {
        /// Not supervising anything. TcrBar has spawned no child.
        case idle
        /// A child was spawned by this app and is alive.
        case supervising(pid: Int32)
        /// The spawn declined because another proxy already holds the port. That
        /// server is *not* ours; it will never be signalled.
        case incumbentHoldsPort(message: String)
        /// A takeover was requested and did NOT happen — the incumbent is still
        /// there. Distinct from ``incumbentHoldsPort`` because on the takeover path
        /// that same condition is a failure, not the benign outcome.
        case takeoverRefused(message: String)
        /// The child exited (or never started). Reported verbatim.
        case exited(exitCode: Int32, message: String)
        /// `tcr` could not be located.
        case toolMissing(searched: [String])

        /// True only when a stop is a legal operation: we spawned this process.
        public var isOurChild: Bool {
            if case .supervising = self { return true }
            return false
        }

        public var summary: String {
            switch self {
            case .idle:
                return "Not supervised by TcrBar"
            case .supervising(let pid):
                return "Supervised by TcrBar (pid \(pid))"
            case .incumbentHoldsPort(let message):
                return "Already running — not ours, left alone. \(message)"
            case .takeoverRefused(let message):
                return """
                    Takeover did not happen — the existing proxy is still serving. \
                    tcr refuses to replace a proxy hosted inside a `tcr run` \
                    process, because killing it would kill the Claude session \
                    running through it. Stop that process from its own terminal \
                    instead; retrying here will not change the outcome. \(message)
                    """
            case .exited(let code, let message):
                let detail = message.isEmpty ? "no output" : message
                return "Server exited (\(code)): \(detail)"
            case .toolMissing(let searched):
                return "tcr not found (searched \(searched.count) locations)"
            }
        }
    }

    @Published public private(set) var state: State = .idle

    private var child: Process?
    private let stderrBuffer = LockedString()

    public init() {}

    /// The two — and only two — argument sets this app will ever spawn.
    ///
    /// `safeArguments` is the default for every routine start. It withholds
    /// `--replace`, so `tcr` refuses the port if another proxy already holds it
    /// and an accidental click cannot cost anything: the worst outcome is
    /// `.incumbentHoldsPort`, which is a report, not damage. `--no-replace` is
    /// kept in the list even though standing down is now `tcr`'s default — the
    /// flag is still accepted (`src/main.rs:192`), and passing it means this app
    /// stays safe against an older `tcr` on `PATH`, where omitting it meant
    /// takeover.
    ///
    /// `takeoverArguments` adds `--replace`, which is precisely what asks `tcr`'s
    /// singleton to replace the incumbent. That kill happens inside `tcr`, never
    /// here. It is reachable only from `startTakingOverPort()`, behind an
    /// explicit confirmation, because it wipes the session→account pin map and
    /// costs every live session a cold prompt-cache prefix.
    /// `--headless` is not optional here, it is what makes the child survive.
    ///
    /// Without it `tcr server` runs its ratatui TUI (`src/main.rs:615`, "the TUI
    /// owns the foreground"), which calls `enable_raw_mode()?` on stdout
    /// (`src/tui.rs:47`). This app spawns with a `Pipe`, so there is no terminal,
    /// raw mode fails, the `?` propagates and the process exits immediately.
    ///
    /// That was a shipped bug and it was not takeover-specific: every spawn path
    /// lacked the flag, so "Start server", "Start server at launch" and the
    /// takeover all launched a server that died on startup. It presented as the
    /// server appearing briefly and vanishing, which reads like a crash in the
    /// proxy rather than a missing argument in its launcher.
    public nonisolated static let safeArguments = ["server", "--headless", "--no-replace"]

    /// Deliberately carries `--replace`, and deliberately never `--no-replace` —
    /// when both are passed `tcr` lets the safe one win (`src/main.rs:486`), so a
    /// takeover set containing both would silently do nothing. See
    /// `safeArguments`. Keeps `--headless` for the same reason every other spawn
    /// does.
    public nonisolated static let takeoverArguments = ["server", "--headless", "--replace"]

    /// The default argument set. Kept as a distinct name so existing call sites
    /// and tests read as "the safe one" rather than "the only one".
    public nonisolated static var serverArguments: [String] { safeArguments }

    public func start() {
        launch(intent: .safeStart, arguments: Self.safeArguments)
    }

    /// Start a server that *replaces* whatever holds the port.
    ///
    /// The replacement is performed by `tcr` itself, as a consequence of the
    /// `--replace` flag. No pid is signalled from Swift. Callers must have
    /// confirmed with the operator first — this is the most expensive action in
    /// the app.
    public func startTakingOverPort() {
        launch(intent: .takeover, arguments: Self.takeoverArguments)
    }

    private func launch(intent: Intent, arguments: [String]) {
        guard child == nil else { return }
        switch TcrTool.resolve() {
        case .failure(let notFound):
            state = .toolMissing(searched: notFound.searched)
        case .success(let executable):
            spawn(executable: executable, intent: intent, arguments: arguments)
        }
    }

    private func spawn(executable: URL, intent: Intent, arguments: [String]) {
        let process = Process()
        process.executableURL = executable
        process.arguments = arguments
        let err = Pipe()
        process.standardError = err
        // Discard stdout — do NOT hand it a Pipe.
        //
        // `--headless` means "log to stdout" (`src/main.rs`), so a spawned server
        // writes a steady stream there. A `Pipe()` nothing reads fills its 64KiB
        // kernel buffer and the next write blocks FOREVER: the proxy wedges
        // mid-serve, still alive, so `terminationHandler` never fires and this app
        // keeps reporting `.supervising`. A hung server displayed as healthy.
        //
        // Only `standardError` gets a Pipe, and it is drained by the
        // `readabilityHandler` below — that is what makes it safe.
        //
        // Nothing is lost: the server's durable log is `$TMPDIR/teamclaude-rs.log`,
        // which is where `tcr status` and every diagnostic already read from.
        //
        // Pairs with ce1cb27 (`--headless`, without which the child died instantly).
        // That fix let the server SURVIVE startup, which is precisely what let it
        // live long enough to reach this second wall.
        process.standardOutput = FileHandle.nullDevice
        stderrBuffer.reset()
        err.fileHandleForReading.readabilityHandler = { [stderrBuffer] handle in
            let data = handle.availableData
            guard !data.isEmpty, let text = String(data: data, encoding: .utf8) else { return }
            stderrBuffer.append(text)
        }
        process.terminationHandler = { [weak self, stderrBuffer] finished in
            let code = finished.terminationStatus
            let text = stderrBuffer.value
            err.fileHandleForReading.readabilityHandler = nil
            Task { @MainActor in
                guard let self else { return }
                self.child = nil
                self.state = Self.classifyExit(intent: intent, exitCode: code, stderr: text)
            }
        }
        do {
            try process.run()
            child = process
            state = .supervising(pid: process.processIdentifier)
        } catch {
            child = nil
            state = .exited(exitCode: -1, message: error.localizedDescription)
        }
    }

    /// Terminate the child **we** spawned. A server we did not start is never
    /// signalled — there is deliberately no code path that can.
    public func stop() {
        guard let process = child, process.isRunning else {
            child = nil
            return
        }
        process.terminate()
    }

    /// Called on app quit so a supervised child does not outlive its supervisor.
    public func terminateSupervisedChildOnQuit() {
        stop()
    }

    /// Markers `tcr` prints when it stands down rather than take the port, or when
    /// the subsequent bind fails because the incumbent is still there.
    ///
    /// `another proxy holds` is the live one: it is `singleton::INCUMBENT_MARKER`,
    /// a named constant on the Rust side precisely because this list is the only
    /// thing reading it, across a language boundary no compiler checks. Note that
    /// a stand-down now exits **0** — the marker, not the exit code, is what says
    /// an incumbent is there, and `classifyExit` matches markers first for exactly
    /// that reason. `--no-replace was set` is the wording an older `tcr` used for
    /// the same refusal, kept so this app still reads a build that predates the
    /// `--replace` flip. `failed to bind` is the anyhow context wrapping the
    /// listener at `src/main.rs:571`. Those three are strings *this project* owns,
    /// so they only move when someone edits those lines. `Address already in use`
    /// is the OS strerror, kept because it can still surface in the anyhow cause
    /// chain — but it is the runtime's wording, not ours, and is never relied on
    /// alone.
    nonisolated static let incumbentMarkers = [
        "--no-replace was set",
        "another proxy holds",
        "failed to bind",
        "Address already in use",
        "address already in use",
    ]

    /// Why the spawn was made. The same stderr means opposite things on the two
    /// paths, so classification cannot be argument-blind.
    public enum Intent: Equatable, Sendable {
        /// No `--replace`. Finding an incumbent is the expected, benign outcome.
        case safeStart
        /// `--replace`. The user explicitly asked to replace the incumbent, so
        /// finding one still there means the request did NOT happen.
        case takeover
    }

    /// Pure classification of a finished spawn.
    ///
    /// On `.safeStart`, "another proxy already holds the port" is the *success*
    /// path: a server is running, it simply is not ours.
    ///
    /// On `.takeover` the identical stderr is a *failure*. This was a real shipped
    /// bug: the user clicked "Take over port", `tcr` declined to replace the
    /// incumbent, the bind then failed with `failed to bind 127.0.0.1:3456`, and
    /// this function — knowing nothing about which arguments were used — reported
    /// the benign "already running" outcome. Nothing was taken over and the panel
    /// said everything was fine.
    ///
    /// The common cause is not transient, so the message says so rather than
    /// inviting a retry: `tcr`'s port singleton deliberately refuses to replace a
    /// proxy hosted inside a `tcr run` process (`src/singleton.rs:38,62`, asserted
    /// at `:257`), because killing it would kill the Claude session running
    /// through it. No number of clicks will change that outcome.
    public nonisolated static func classifyExit(
        intent: Intent = .safeStart,
        exitCode: Int32,
        stderr: String
    ) -> State {
        let trimmed = stderr.trimmingCharacters(in: .whitespacesAndNewlines)
        if incumbentMarkers.contains(where: { trimmed.contains($0) }) {
            switch intent {
            case .safeStart:
                return .incumbentHoldsPort(message: trimmed)
            case .takeover:
                return .takeoverRefused(message: trimmed)
            }
        }
        return .exited(exitCode: exitCode, message: trimmed)
    }
}

/// Minimal thread-safe string accumulator for the child's stderr, which arrives
/// on a `readabilityHandler` queue.
final class LockedString: @unchecked Sendable {
    private let lock = NSLock()
    private var storage = ""

    var value: String {
        lock.lock()
        defer { lock.unlock() }
        return storage
    }

    func append(_ text: String) {
        lock.lock()
        storage += text
        lock.unlock()
    }

    func reset() {
        lock.lock()
        storage = ""
        lock.unlock()
    }
}
