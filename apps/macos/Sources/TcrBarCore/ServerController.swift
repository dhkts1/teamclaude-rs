import Combine
import Foundation

/// Supervises a `tcr server` child process.
///
/// The safety property this type exists to hold:
///
/// `tcr`'s port singleton is a *takeover* by default — a starting server kills a
/// recognised proxy already holding the port, because two proxies mutually
/// invalidate each other's single-use OAuth refresh tokens. That kill also wipes
/// the session→account pin map, and Anthropic's prompt cache is per-account, so
/// every live session pays a full cold prefix. It is the most expensive event in
/// this system.
///
/// So: the routine path *always* spawns with `--no-replace`, and this controller
/// signals **only** a child it spawned itself. An incumbent server it merely
/// observed is never touched, and failing to start because one already holds the
/// port is reported as "already running" — a success, not an error.
///
/// There is one deliberate exception, `startTakingOverPort()`, and it is still
/// not a signal: it spawns `tcr server` *without* `--no-replace` and lets `tcr`'s
/// own singleton (`src/singleton.rs`) do the replacing, inside the code written
/// to do it safely. This app never sends a signal to a pid it did not spawn, and
/// there is deliberately no code path here that could.
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
    /// `safeArguments` is the default for every routine start. `--no-replace`
    /// makes `tcr` refuse the port if another proxy already holds it, so an
    /// accidental click cannot cost anything: the worst outcome is
    /// `.incumbentHoldsPort`, which is a report, not damage.
    ///
    /// `takeoverArguments` omits that flag, which is precisely what asks `tcr`'s
    /// singleton to replace the incumbent. That kill happens inside `tcr`, never
    /// here. It is reachable only from `startTakingOverPort()`, behind an
    /// explicit confirmation, because it wipes the session→account pin map and
    /// costs every live session a cold prompt-cache prefix.
    public nonisolated static let safeArguments = ["server", "--no-replace"]

    /// Deliberately omits `--no-replace`. See `safeArguments`.
    public nonisolated static let takeoverArguments = ["server"]

    /// The default argument set. Kept as a distinct name so existing call sites
    /// and tests read as "the safe one" rather than "the only one".
    public nonisolated static var serverArguments: [String] { safeArguments }

    public func start() {
        launch(arguments: Self.safeArguments)
    }

    /// Start a server that *replaces* whatever holds the port.
    ///
    /// The replacement is performed by `tcr` itself, as a consequence of the
    /// missing `--no-replace`. No pid is signalled from Swift. Callers must have
    /// confirmed with the operator first — this is the most expensive action in
    /// the app.
    public func startTakingOverPort() {
        launch(arguments: Self.takeoverArguments)
    }

    private func launch(arguments: [String]) {
        guard child == nil else { return }
        switch TcrTool.resolve() {
        case .failure(let notFound):
            state = .toolMissing(searched: notFound.searched)
        case .success(let executable):
            spawn(executable: executable, arguments: arguments)
        }
    }

    private func spawn(executable: URL, arguments: [String]) {
        let process = Process()
        process.executableURL = executable
        process.arguments = arguments
        let err = Pipe()
        process.standardError = err
        process.standardOutput = Pipe()
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
                self.state = Self.classifyExit(exitCode: code, stderr: text)
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

    /// Markers `tcr` prints when `--no-replace` keeps it from taking the port, or
    /// when the subsequent bind fails because the incumbent is still there.
    ///
    /// The first two come from the `--no-replace` refusal at `src/singleton.rs:146`;
    /// `failed to bind` is the anyhow context wrapping the listener at
    /// `src/main.rs:505`. Those three are strings *this project* owns, so they only
    /// move when someone edits those lines. `Address already in use` is the OS
    /// strerror, kept because it can still surface in the anyhow cause chain — but
    /// it is the runtime's wording, not ours, and is never relied on alone.
    nonisolated static let incumbentMarkers = [
        "--no-replace was set",
        "another proxy holds",
        "failed to bind",
        "Address already in use",
        "address already in use",
    ]

    /// Pure classification of a finished spawn.
    ///
    /// "Another proxy already holds the port" is the *success* path: a server is
    /// running, it simply is not ours.
    public nonisolated static func classifyExit(exitCode: Int32, stderr: String) -> State {
        let trimmed = stderr.trimmingCharacters(in: .whitespacesAndNewlines)
        if incumbentMarkers.contains(where: { trimmed.contains($0) }) {
            return .incumbentHoldsPort(message: trimmed)
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
