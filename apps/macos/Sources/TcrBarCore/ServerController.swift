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
        /// A process holds the port and answered nothing. It is not serving, and
        /// it is not ours to signal. The most alarming state this app can report:
        /// every session pointed at the port is failing right now.
        case incumbentNotAnswering(message: String)
        /// The incumbent is serving, but from a different build than the one on
        /// disk. Benign next to ``incumbentNotAnswering`` — the proxy works, it is
        /// merely old — so it is a note, not an alarm.
        case incumbentIsStale(message: String)
        /// The child exited (or never started). Reported verbatim.
        case exited(exitCode: Int32, message: String)
        /// `tcr` could not be located.
        case toolMissing(searched: [String])
        /// The `tcr` on disk predates a flag this app needs, so it rejected the
        /// spawn with a clap usage error. Distinct from ``exited`` because the
        /// remedy is "update tcr", not "read this stack trace".
        case toolTooOld(message: String)

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
            case .incumbentNotAnswering(let message):
                return """
                    NOT SERVING — a process holds the port but answered nothing, \
                    so requests through it are failing. It was left alone rather \
                    than replaced: a proxy that was merely slow to answer would \
                    lose its session→account pin map, so this app will not make \
                    that call for you. "Take over port…" is the recovery. If a \
                    takeover is what produced this, tcr declined to replace this \
                    particular holder — a proxy hosted inside a `tcr run` process \
                    is deliberately never replaced — and it has to be stopped from \
                    its own terminal. \(message)
                    """
            case .incumbentIsStale(let message):
                return """
                    Already running — not ours, left alone. It is serving an older \
                    build than the `tcr` on disk, so a fix you just built is not \
                    live on the port. "Take over port…" replaces it, at the cost of \
                    every live session's prompt cache. \(message)
                    """
            case .exited(let code, let message):
                let detail = message.isEmpty ? "no output" : message
                return "Server exited (\(code)): \(detail)"
            case .toolMissing(let searched):
                return "tcr not found (searched \(searched.count) locations)"
            case .toolTooOld(let message):
                return """
                    The `tcr` on this machine is too old for this action — it does \
                    not accept `--replace`, the flag that asks its port singleton \
                    to replace the incumbent. Update it (`tcr update`, or install \
                    from a current build of this repository) and try again. \(message)
                    """
            }
        }
    }

    @Published public private(set) var state: State = .idle

    private var child: Process?
    /// A capability probe is in flight for the takeover path. Guards the window
    /// between the click and the spawn, where `child` is still nil and a second
    /// click would otherwise start a second server.
    private var probing = false

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
    /// the two are a hard clap conflict (`src/main.rs:198`,
    /// `#[arg(long, conflicts_with = "replace")]`), so a takeover set containing
    /// both never reaches `tcr`'s own logic at all: clap rejects the pair by name
    /// and exits 2 before the server starts. The rationale for making it an error
    /// rather than a precedence rule is in that argument's own doc-comment
    /// (`src/main.rs:191-197`): the previous wiring quietly discarded `--replace`,
    /// which is the one outcome an operator cannot tell apart from success. See
    /// `safeArguments`. Keeps `--headless` for the same reason every other spawn
    /// does.
    public nonisolated static let takeoverArguments = ["server", "--headless", "--replace"]

    /// How a `tcr` that predates the `--replace` flag is asked to take the port.
    ///
    /// On such a build taking over was the *default*, and `--no-replace` was the
    /// only way to decline it — so the takeover is expressed by passing neither
    /// flag. `--replace` does not exist there, and clap rejects unknown arguments
    /// with a usage error and exit code 2, which is why the modern set cannot
    /// simply be sent and hoped for.
    public nonisolated static let legacyTakeoverArguments = ["server", "--headless"]

    /// Whether the `tcr` we are about to spawn understands `--replace`.
    public enum ReplaceFlagSupport: Equatable, Sendable {
        case supported
        case unsupported
    }

    /// The takeover argument set for a given binary vintage.
    ///
    /// Both spellings really do take the port on their own vintage, so gating on
    /// the flag keeps the button working across the flip rather than trading one
    /// broken half for the other. Both mistakes are safe by construction: modern
    /// arguments sent to an old binary produce a usage error that ``classifyExit``
    /// names as "your tcr is too old", and legacy arguments sent to a modern one
    /// make it stand down, which is `.takeoverRefused` — a visible failure, never
    /// an unwanted kill.
    public nonisolated static func takeoverArgumentSet(
        _ support: ReplaceFlagSupport
    ) -> [String] {
        switch support {
        case .supported: return takeoverArguments
        case .unsupported: return legacyTakeoverArguments
        }
    }

    /// Read `--replace` support out of `tcr server --help`.
    ///
    /// Matching is on whole tokens. `"--no-replace"` does *not* contain
    /// `"--replace"` — there is one hyphen before `replace`, not two — so the
    /// obvious trap is not the live one; the live one is any *longer* flag
    /// beginning with the same characters. A future `--replace-if-stale` would
    /// make `text.contains("--replace")` report support for a flag the binary does
    /// not have, which is the failure this whole probe exists to prevent, arrived
    /// at from the other side.
    ///
    /// The default is `.supported`, and `.unsupported` is returned only on
    /// positive evidence — help that offers `--no-replace` and no `--replace`.
    /// Unreadable help means "assume modern": that path ends in a usage error this
    /// controller explains, whereas guessing "old" against a modern binary would
    /// send arguments that make it stand down instead of taking the port.
    public nonisolated static func replaceFlagSupport(inHelpText text: String) -> ReplaceFlagSupport {
        let tokens = Set(
            text.components(separatedBy: CharacterSet(charactersIn: " \t\n\r,;[]<>=()"))
        )
        if tokens.contains("--replace") { return .supported }
        if tokens.contains("--no-replace") { return .unsupported }
        return .supported
    }

    /// Ask the binary itself. `--help` is answered by clap before any of `tcr`'s
    /// own code runs, so this never binds a port, never signals anything and costs
    /// one short-lived process.
    ///
    /// Blocking, so it is called off the main actor. Any failure to run or decode
    /// falls back to `.supported`, for the reason given on ``replaceFlagSupport``.
    nonisolated static func probeReplaceFlagSupport(executable: URL) -> ReplaceFlagSupport {
        guard let output = try? TcrTool.run(executable: executable, arguments: ["server", "--help"])
        else { return .supported }
        let text = (String(data: output.stdout, encoding: .utf8) ?? "") + output.stderr
        return replaceFlagSupport(inHelpText: text)
    }

    /// How long the capability probe may take before the takeover proceeds
    /// without it. `--help` is a parse and a write; anything slower is not a slow
    /// answer, it is no answer.
    static let probeTimeout: Double = 5

    /// Run a probe under a deadline, falling back to `.supported` when it does not
    /// answer in time.
    ///
    /// Without this, one unanswerable `--help` disables the takeover button for
    /// the lifetime of the app: `probing` would stay true forever and every later
    /// click would return at the guard, silently. `TCR_BIN` and the defaults key
    /// let an operator point `tcr` at an arbitrary executable, so "the binary
    /// always answers" is not something this app gets to assume.
    ///
    /// The abandoned probe is not cancellable mid-read; it is simply no longer
    /// waited on. It holds no port and signals nothing.
    ///
    /// Deliberately *not* a `withTaskGroup` race, which is the obvious shape and
    /// does not work: a task group awaits every child before it returns, so
    /// `cancelAll()` cannot abandon a subprocess read that ignores cancellation —
    /// the group returns the timeout's answer only once the probe has finished
    /// anyway, which is the hang it was supposed to prevent. The first version of
    /// this function did exactly that and took 30 seconds to honour a 0.2-second
    /// deadline. The probe therefore runs on a Dispatch queue, off the cooperative
    /// pool entirely, and is waited on by a semaphore that has its own deadline.
    static func support(
        within seconds: Double,
        probe: @escaping @Sendable () -> ReplaceFlagSupport
    ) async -> ReplaceFlagSupport {
        let answer = LockedSupport()
        let finished = DispatchSemaphore(value: 0)
        DispatchQueue.global(qos: .userInitiated).async {
            answer.value = probe()
            finished.signal()
        }
        return await Task.detached(priority: .userInitiated) {
            _ = finished.wait(timeout: .now() + seconds)
            return answer.value ?? .supported
        }.value
    }

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
    ///
    /// Unlike ``start()`` this asks the binary what it accepts before spawning,
    /// because the flag that expresses "take the port" changed spelling and
    /// TcrBar is routinely newer than the `tcr` on `PATH` — the app and the CLI
    /// are separate installs. The probe runs off the main actor; the spawn itself
    /// happens back on it.
    public func startTakingOverPort() {
        guard child == nil, !probing else { return }
        switch TcrTool.resolve() {
        case .failure(let notFound):
            state = .toolMissing(searched: notFound.searched)
        case .success(let executable):
            probing = true
            Task { [weak self] in
                let support = await Self.support(within: Self.probeTimeout) {
                    Self.probeReplaceFlagSupport(executable: executable)
                }
                guard let self else { return }
                self.probing = false
                guard self.child == nil else { return }
                self.spawn(
                    executable: executable,
                    intent: .takeover,
                    arguments: Self.takeoverArgumentSet(support)
                )
            }
        }
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
        // Only `standardError` gets a Pipe, and `ChildStderr` drains it
        // continuously — that is what makes it safe.
        //
        // Nothing is lost: the server's durable log is `$TMPDIR/teamclaude-rs.log`,
        // which is where `tcr status` and every diagnostic already read from.
        //
        // Pairs with ce1cb27 (`--headless`, without which the child died instantly).
        // That fix let the server SURVIVE startup, which is precisely what let it
        // live long enough to reach this second wall.
        process.standardOutput = FileHandle.nullDevice
        let stderr = ChildStderr(reading: err)
        process.terminationHandler = { [weak self] finished in
            let code = finished.terminationStatus
            let text = stderr.finish()
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

    /// The stand-down exit codes `tcr` defines at `src/main.rs:479-494`.
    ///
    /// A stand-down no longer always means exit 0. These two codes carry facts
    /// about the incumbent that the *markers cannot*, because a stand-down prints
    /// the same "another proxy holds" line in all three cases — only the code
    /// separates a healthy incumbent from a wedged one.
    ///
    /// Keying on the code rather than on the prose is deliberate and matches the
    /// Rust side's own reasoning: a verdict grepped out of a sentence is disarmed
    /// by any rewording. It also survives a lost stderr, which is not theoretical
    /// here — see ``ChildStderr``.
    ///
    /// Neither code can arrive from a `tcr` predating them: those builds exit 0,
    /// 1 or 2, so reading 3 and 4 costs nothing in version-robustness.
    enum StandDownExit {
        /// The incumbent is serving an older build than the binary just run.
        static let stale: Int32 = 3
        /// The incumbent never answered the liveness probe. It holds the socket
        /// and serves nothing.
        static let notAnswering: Int32 = 4
    }

    /// What clap prints when it is handed an argument the binary does not define.
    ///
    /// Measured against clap 4 with this project's own `ServerArgs` wiring rather
    /// than recalled: `error: unexpected argument '--replace' found`, followed by
    /// `tip: a similar argument exists: '--no-replace'`. The clap 3 spellings are
    /// kept as cheap insurance, since the binary is whatever `tcr` is installed
    /// and this app does not get to pin its version.
    ///
    /// Deliberately excludes the *conflict* wording. `--replace` together with
    /// `--no-replace` is now a hard clap conflict which also exits 2, and it reads
    /// `error: the argument '--replace' cannot be used with '--no-replace'` —
    /// measured, same run. Matching that as "your tcr is too old" would tell an
    /// operator to rebuild a perfectly current binary. It carries none of the
    /// markers below, so it falls through to a verbatim `.exited(2, …)`, which is
    /// the honest report: no path in this app builds both flags, so seeing it at
    /// all would mean a bug here, not a stale CLI.
    nonisolated static let unknownArgumentMarkers = [
        "unexpected argument",
        "wasn't expected",
        "isn't valid in this context",
        "Found argument",
        "unrecognized",
    ]

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
    ///
    /// A third outcome sits ahead of both: a `tcr` old enough to reject the
    /// arguments outright. See ``unknownArgumentMarkers``.
    ///
    /// And ahead of the markers sit the stand-down exit codes. Every stand-down
    /// prints the same "another proxy holds" line, so on the marker alone all
    /// three are indistinguishable and all three read as the benign "already
    /// running". For exit 4 that is the inverse of the truth — the port is held by
    /// something serving nothing — and a panel that says a wedged proxy is fine is
    /// worse than one that says nothing at all. See ``StandDownExit``.
    public nonisolated static func classifyExit(
        intent: Intent = .safeStart,
        exitCode: Int32,
        stderr: String
    ) -> State {
        let trimmed = stderr.trimmingCharacters(in: .whitespacesAndNewlines)
        // Checked first, and before the incumbent markers, because it is the more
        // specific claim: this binary never even parsed the request. Reporting it
        // as a bare `.exited(2, …)` would show the operator a raw clap usage dump
        // right after they confirmed a destructive alert, with no hint that the
        // remedy is to update `tcr` rather than to retry.
        if trimmed.contains("--replace"),
            unknownArgumentMarkers.contains(where: { trimmed.contains($0) })
        {
            return .toolTooOld(message: trimmed)
        }
        // Both intents, deliberately. On the takeover path these are still the
        // more useful facts than "refused": exit 4 says the thing that kept the
        // port is not serving, which `.takeoverRefused`'s wording ("the existing
        // proxy is still serving") would flatly contradict.
        if exitCode == StandDownExit.notAnswering {
            return .incumbentNotAnswering(message: trimmed)
        }
        if exitCode == StandDownExit.stale {
            return .incumbentIsStale(message: trimmed)
        }
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

/// Collects a child process's stderr, and — this is the point of the type —
/// **drains the pipe** when the child exits instead of merely snapshotting what
/// happened to have arrived.
///
/// Foundation delivers pipe readability on a background queue with no ordering
/// guarantee against `terminationHandler`. A `tcr` that writes its stand-down
/// message and exits in the same breath can therefore die with its final chunk
/// still unread, and the previous code took its snapshot at that moment and then
/// discarded the rest by clearing the handler. Measured on this machine over 400
/// spawns of a child writing 600 bytes and exiting: 10 reads came back empty or
/// truncated (a second run: 9). With the drain below, 0 of 400, twice.
///
/// Empty stderr is not a harmless loss here. A stand-down exits **0** since the
/// `--replace` flip, so a lost message classifies as `.exited(0, "")` and the
/// panel renders "Server exited (0): no output" — TcrBar reporting a clean start
/// and stop while an incumbent proxy is still serving, which is verbatim the
/// misreport `incumbentMarkers` exists to prevent. The old non-zero refusal made
/// the same race at least *look* like an error.
///
/// `readToEnd()` blocks until EOF, which arrives when the last writer closes.
/// `Process` closes the parent's copy of the write end at spawn, and `tcr server`
/// forks nothing that could inherit it, so the only writer is the child that has
/// already exited. `TcrTool.run` makes the identical assumption on every status
/// poll (`readDataToEndOfFile()`), so this is the codebase's existing contract
/// with the CLI, not a new one.
final class ChildStderr: @unchecked Sendable {
    private let handle: FileHandle
    private let buffer = LockedString()
    private let streaming: Bool

    /// Signalled by the streaming callback when it reaches EOF, and waited on by
    /// ``finish()``. Draining the pipe is not enough on its own: the callback that
    /// took the child's bytes out of the pipe runs on Foundation's queue, and
    /// `readabilityHandler = nil` does not wait for a callback already inside its
    /// body. A callback that has returned from `availableData` but not yet reached
    /// `buffer.append` holds the entire message in a local — invisible to both the
    /// buffer and the pipe. `finish()` then drains an empty pipe and returns an
    /// empty buffer while the bytes are in flight, which is why the loss is
    /// all-or-nothing rather than a truncated tail.
    ///
    /// Measured on this machine, unchanged test, 1000 spawns: 9 empty. With a 50ms
    /// delay inserted between `availableData` and `append` — which only widens the
    /// window, it does not create it — 19 of 20 empty. With this wait: 0 of 2000,
    /// and 0 of 20 under the same 50ms delay.
    ///
    /// Foundation serialises one handle's readability callbacks, so the EOF
    /// callback cannot overtake the data callback that precedes it. Waiting for
    /// EOF therefore happens-after every `append`. That ordering is what makes this
    /// a fix rather than a narrower window.
    private let drained = DispatchSemaphore(value: 0)

    /// How long ``finish()`` will wait for that EOF before giving up.
    ///
    /// Bounded, because EOF needs every writer to close: a child that forked a
    /// grandchild holding the inherited write end would never produce one, and an
    /// unbounded wait would hang `terminationHandler` — trading lost stderr for a
    /// UI that never leaves `.supervising`. On expiry the behaviour degrades to
    /// exactly what it was before this wait existed, never worse. `tcr server`
    /// forks nothing, so the timeout is a backstop, not a code path: the wait
    /// measured at most 12ms over 2000 spawns.
    private static let eofWait: DispatchTimeInterval = .seconds(2)

    /// - Parameter installReadabilityHandler: false only in tests. It reproduces,
    ///   deterministically, the state the race produces by accident — bytes in the
    ///   pipe that the readability callback has not consumed — so a regression
    ///   here fails every run rather than one run in forty.
    init(reading pipe: Pipe, installReadabilityHandler: Bool = true) {
        handle = pipe.fileHandleForReading
        streaming = installReadabilityHandler
        guard installReadabilityHandler else { return }
        handle.readabilityHandler = { [buffer, drained] handle in
            let data = handle.availableData
            // Empty means EOF: the child exited and every writer has closed. It is
            // delivered after the callbacks carrying the data, so signalling here
            // tells `finish()` that nothing is still in flight.
            guard !data.isEmpty else {
                drained.signal()
                return
            }
            guard let text = String(data: data, encoding: .utf8) else { return }
            buffer.append(text)
        }
    }

    /// Call once, after the child has exited. Stops the streaming reader, takes
    /// whatever is still in the pipe, and returns everything the child wrote.
    ///
    /// A callback already in flight cannot duplicate text — a read consumes the
    /// bytes it returns, so each byte reaches the buffer exactly once. It can in
    /// principle interleave two chunks out of order; every consumer of this string
    /// is a substring match, so ordering is not load-bearing.
    func finish() -> String {
        // Let the streaming reader finish handing over what it already took out of
        // the pipe. Without this the drain below can run while the child's only
        // chunk sits in a callback local, and this returns "". See ``drained``.
        if streaming { _ = drained.wait(timeout: .now() + Self.eofWait) }
        handle.readabilityHandler = nil
        if let tail = try? handle.readToEnd(), !tail.isEmpty,
            let text = String(data: tail, encoding: .utf8)
        {
            buffer.append(text)
        }
        return buffer.value
    }
}

/// A capability answer written by a probe thread and read by whoever is still
/// waiting when the deadline passes. `nil` means "no answer yet".
final class LockedSupport: @unchecked Sendable {
    private let lock = NSLock()
    private var storage: ServerController.ReplaceFlagSupport?

    var value: ServerController.ReplaceFlagSupport? {
        get {
            lock.lock()
            defer { lock.unlock() }
            return storage
        }
        set {
            lock.lock()
            storage = newValue
            lock.unlock()
        }
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
