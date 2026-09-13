import AppKit
import Combine
import Foundation

/// Runs `tcr login --non-interactive` as a child of this app, so signing in is
/// something the panel does rather than something it hands to a Terminal
/// window.
///
/// ## Why this can exist now, when ``LoginLauncher`` could not
///
/// `LoginLauncher`'s doc-comment names two reasons a GUI could not spawn
/// `tcr login`, and both are gone for this path:
///
///  1. A modern proxy takes a login live while serving (`a385f0f`), so the
///     port no longer refuses one.
///  2. The CLI was interactive — a name prompt and a pasted-code fallback,
///     both on a stdin a background child does not have. `--non-interactive`
///     removes both: the loopback callback is the only completion path, and
///     the name is the identity the browser returns.
///
/// The browser is opened HERE, not by the CLI, because only this app can bring
/// its own window forward afterwards — the flag prints the URL and leaves it
/// to the caller for exactly that reason.
///
/// ``LoginLauncher`` stays for `tcr mint` (a different flow, still
/// interactive) and as the fallback when the `tcr` on this machine predates
/// the flag — see ``LoginCapability``.
@MainActor
public final class LoginSession: ObservableObject {

    /// Spelled out as ``LoginPhase``, which is deliberately NOT nested: this
    /// class is `@MainActor` and a nested type would inherit that isolation,
    /// putting the state machine — a plain value type — behind the main actor
    /// for no reason.
    public typealias Phase = LoginPhase

    @Published public private(set) var phase: Phase = .opening
    /// The URL the browser was sent to, kept for the "Copy link" button: the
    /// one recovery when the default browser did not come forward.
    @Published public private(set) var authorizeURL: URL?

    private var flow: LoginFlow
    private var process: Process?
    private var buffer = LineBuffer()
    private var stderrText = ""
    private var cancelled = false

    private let account: String?
    private let openURL: (URL) -> Void
    private let resolve: () -> Result<URL, TcrTool.NotFound>

    /// `account` is the row a re-login targets, `nil` for a fresh add. `open`
    /// and `resolve` are injectable so a test can drive the whole session
    /// without a browser or an installed `tcr`.
    public init(
        account: String? = nil,
        open: @escaping (URL) -> Void = { NSWorkspace.shared.open($0) },
        resolve: @escaping () -> Result<URL, TcrTool.NotFound> = { TcrTool.resolve() }
    ) {
        self.account = account
        self.openURL = open
        self.resolve = resolve
        self.flow = LoginFlow(requesting: account)
    }

    /// `tcr login --non-interactive [--account <name>]`. Passed as argv, never
    /// through a shell, so nothing here needs quoting — unlike
    /// ``LoginLauncher/script(forExecutableAt:reloggingIn:)``, which composes a
    /// real `.command` file and must.
    ///
    /// `--force` is never passed, for the same reason `LoginLauncher` never
    /// passes it: a GUI silently forcing past a guard the CLI put up is the
    /// wrong use of a button.
    public nonisolated static func arguments(account: String?) -> [String] {
        var arguments = ["login", "--non-interactive"]
        if let account {
            arguments.append(contentsOf: ["--account", account])
        }
        return arguments
    }

    /// Spawn the child and start reading its events. Safe to call once; a
    /// second call while one is in flight does nothing.
    public func start() {
        guard process == nil, !phase.isTerminal else { return }

        let executable: URL
        switch resolve() {
        case .success(let url): executable = url
        case .failure(let missing):
            phase = .failed(
                reason: "tcr not found (searched \(missing.searched.count) locations). "
                    + TcrTool.overrideRemedy)
            return
        }

        let child = Process()
        child.executableURL = executable
        child.arguments = Self.arguments(account: account)
        let out = Pipe()
        let err = Pipe()
        child.standardOutput = out
        child.standardError = err
        // No stdin at all, rather than an inherited one: `--non-interactive`
        // never reads it, and a child that inherits the GUI's descriptor is
        // one CLI change away from blocking on a prompt nobody can answer.
        child.standardInput = FileHandle.nullDevice

        out.fileHandleForReading.readabilityHandler = { [weak self] handle in
            let data = handle.availableData
            guard !data.isEmpty else { return }
            Task { @MainActor [weak self] in self?.ingest(data) }
        }
        err.fileHandleForReading.readabilityHandler = { [weak self] handle in
            let data = handle.availableData
            guard !data.isEmpty else { return }
            let text = String(data: data, encoding: .utf8) ?? ""
            Task { @MainActor [weak self] in self?.stderrText += text }
        }
        child.terminationHandler = { [weak self] finished in
            Task { @MainActor [weak self] in
                self?.childEnded(exitCode: finished.terminationStatus)
            }
        }

        do {
            try child.run()
            process = child
        } catch {
            phase = .failed(reason: "could not run \(executable.path): \(error.localizedDescription)")
        }
    }

    /// Stop the login. The child is terminated rather than left running: it
    /// holds a loopback listener and would otherwise sit there for the rest of
    /// its 2-minute timeout, ready to accept a callback for a flow the person
    /// just abandoned.
    public func cancel() {
        cancelled = true
        process?.terminate()
        process = nil
    }

    /// One chunk of the child's stdout. A chunk is not a line — the split and
    /// the carry-over live in ``LineBuffer`` so a URL arriving in two reads is
    /// still one event.
    private func ingest(_ data: Data) {
        for line in buffer.append(data) {
            guard let event = LoginProgressEvent.parse(line) else { continue }
            if let url = flow.apply(event) {
                authorizeURL = url
                openURL(url)
            }
            phase = flow.phase
        }
    }

    /// The child exited. A terminal phase already tells the whole story; this
    /// is for the case where it did not — a crash, a `terminate()`, or an exit
    /// that said nothing — which would otherwise leave the sheet spinning
    /// forever on a process that is gone.
    private func childEnded(exitCode: Int32) {
        process = nil
        guard !cancelled else { return }
        flow.finish(exitCode: exitCode, stderr: stderrText)
        phase = flow.phase
    }
}

/// What the sheet draws. Four states, no "idle": a session exists only while a
/// login is in flight, and is discarded when the sheet closes.
public enum LoginPhase: Equatable, Sendable {
    /// Started, and nothing has come back yet.
    case opening
    /// The authorize URL is out and the callback is what settles this.
    /// `email` is the identity this login was aimed at (`--account`), not one
    /// the CLI reported — a fresh add has none to name yet.
    case waitingForBrowser(email: String?)
    /// The account is written, under this name.
    case saved(account: String)
    /// Nothing was written, and this is why, on one line.
    case failed(reason: String)

    /// Whether the login is over either way. A terminal phase is what stops
    /// the sheet spinning.
    public var isTerminal: Bool {
        switch self {
        case .opening, .waitingForBrowser: return false
        case .saved, .failed: return true
        }
    }
}

/// One line of `tcr login --non-interactive`'s stdout, parsed.
///
/// The wire format is `LoginEvent::line` in `src/oauth.rs` — one JSON object
/// per line, `event` naming which.
public enum LoginProgressEvent: Equatable, Sendable {
    /// Open this URL. Nothing has been read or written yet.
    case browser(url: String)
    /// The URL is out; the loopback callback is what settles it now.
    case waiting
    /// Written, under this account name.
    case saved(account: String)
    /// Nothing was written, and this is the whole reason.
    case failed(reason: String)

    /// `nil` for anything that is not one of the four events: a blank line, a
    /// line of prose, a JSON object with an `event` this version does not
    /// know, or an event missing the field that carries its meaning.
    ///
    /// Ignoring rather than failing is deliberate. A parser that treated an
    /// unrecognised line as an error would turn any future diagnostic line —
    /// or a `tcr` newer than this app — into a failed login. The one thing a
    /// stray line must never do is change the state.
    public static func parse(_ line: String) -> LoginProgressEvent? {
        let trimmed = line.trimmingCharacters(in: .whitespacesAndNewlines)
        guard trimmed.hasPrefix("{"), let data = trimmed.data(using: .utf8),
            let object = try? JSONSerialization.jsonObject(with: data),
            let fields = object as? [String: Any],
            let event = fields["event"] as? String
        else { return nil }

        func text(_ key: String) -> String? {
            guard let value = fields[key] as? String, !value.isEmpty else { return nil }
            return value
        }

        switch event {
        case "browser":
            guard let url = text("url") else { return nil }
            return .browser(url: url)
        case "waiting":
            return .waiting
        case "saved":
            guard let account = text("account") else { return nil }
            return .saved(account: account)
        case "error":
            guard let reason = text("reason") else { return nil }
            return .failed(reason: reason)
        default:
            return nil
        }
    }
}

/// The state machine behind ``LoginSession``, with no `Process` in it, so the
/// whole sequence — including the ways it ends badly — is testable.
public struct LoginFlow {
    /// The account this login was aimed at, carried into
    /// ``LoginSession/Phase/waitingForBrowser(email:)`` so the sheet can say
    /// who is being signed in.
    public let requested: String?
    public private(set) var phase: LoginPhase = .opening

    public init(requesting: String? = nil) {
        self.requested = requesting
    }

    /// Fold one event in. Returns the URL the caller must open, and only on
    /// the event that asks for one — the side effect stays outside the machine.
    ///
    /// A late event after a terminal phase is ignored: once a login has said
    /// `saved` or `error`, nothing that follows can unsay it.
    @discardableResult
    public mutating func apply(_ event: LoginProgressEvent) -> URL? {
        guard !phase.isTerminal else { return nil }
        switch event {
        case .browser(let url):
            // An unopenable URL is a failure, not a silently skipped step:
            // the browser is the only human part of this flow.
            guard let parsed = URL(string: url), parsed.scheme == "https" else {
                phase = .failed(reason: "tcr printed an authorize URL this app cannot open: \(url)")
                return nil
            }
            return parsed
        case .waiting:
            phase = .waitingForBrowser(email: requested)
            return nil
        case .saved(let account):
            phase = .saved(account: account)
            return nil
        case .failed(let reason):
            phase = .failed(reason: reason)
            return nil
        }
    }

    /// The child process ended. Only meaningful when it ended without saying
    /// how — an exit code and whatever reached stderr are all there is to go
    /// on, and "it stopped" must still become a visible failure rather than a
    /// spinner that never resolves.
    public mutating func finish(exitCode: Int32, stderr: String) {
        guard !phase.isTerminal else { return }
        let spoken = stderr.split(separator: "\n").last.map(String.init)?
            .trimmingCharacters(in: .whitespaces)
        if let spoken, !spoken.isEmpty {
            phase = .failed(reason: spoken)
        } else {
            phase = .failed(reason: "tcr login exited (code \(exitCode)) without saving an account")
        }
    }
}

/// Splits a byte stream into lines, keeping whatever the last read cut in
/// half.
///
/// A pipe read boundary lands wherever the scheduler puts it, so the authorize
/// URL — the longest line this stream carries by a wide margin — is exactly
/// the one likely to arrive in two pieces. Treating each read as a line loses
/// it.
public struct LineBuffer {
    private var carry = Data()

    public init() {}

    /// Every COMPLETE line in `data`, with any trailing partial line held for
    /// the next call.
    public mutating func append(_ data: Data) -> [String] {
        carry.append(data)
        var lines: [String] = []
        while let newline = carry.firstIndex(of: UInt8(ascii: "\n")) {
            let line = carry[carry.startIndex..<newline]
            lines.append(String(decoding: line, as: UTF8.self))
            carry = carry[carry.index(after: newline)...]
        }
        // Re-base the slice so the buffer does not keep growing its indices
        // across a long stream.
        carry = Data(carry)
        return lines
    }
}

/// Whether the `tcr` on this machine has `login --non-interactive` at all.
///
/// TcrBar and `tcr` update independently — the app resolves whichever binary
/// is on this machine (``TcrTool/resolve()``), which can be older than the app
/// that bundles it if someone put one on `PATH`. An app that assumed the flag
/// would spawn a child that exits immediately on an unknown argument, and the
/// person would see "exited without saving an account" where they used to see
/// a Terminal window that worked.
public enum LoginCapability {
    /// The answer, for one `tcr`'s `login --help` text. Pure so the parse is
    /// testable without a binary.
    public static func supportsNonInteractive(help: String) -> Bool {
        help.contains("--non-interactive")
    }

    private static var cached: [String: Bool] = [:]

    /// Ask a binary once and remember. Keyed by path: two different `tcr`s can
    /// answer differently, and the resolved path is what distinguishes them.
    ///
    /// A failed probe answers `false` — the Terminal hand-off works against
    /// every `tcr` ever shipped, so an unreadable answer must fall back to it
    /// rather than onto a flag that may not exist.
    @MainActor
    public static func probe(executable: URL) -> Bool {
        if let known = cached[executable.path] { return known }
        let answer: Bool
        do {
            let output = try TcrTool.run(
                executable: executable, arguments: ["login", "--help"])
            let help = (String(data: output.stdout, encoding: .utf8) ?? "") + output.stderr
            answer = output.exitCode == 0 && supportsNonInteractive(help: help)
        } catch {
            answer = false
        }
        cached[executable.path] = answer
        return answer
    }
}
