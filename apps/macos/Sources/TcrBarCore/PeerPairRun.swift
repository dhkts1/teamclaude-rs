import Foundation

/// One live `tcr peer pair --json`, for the length of one Trust sheet.
///
/// # Why this is not ``PeerController/run(_:)``
///
/// Every other verb this panel runs is one shot: exec, wait, read the exit
/// code, re-read `tcr peer ls --json`. Pairing is not. It is one handshake
/// held open across two phases in ONE process, it can sit for ten minutes
/// waiting for a person at the other Mac, and halfway through it asks this
/// panel a question on stdin. `TcrTool.run` gives a child no stdin at all and
/// blocks until it exits, so a Trust press through it was a subprocess sitting
/// on a `read_line` nobody could answer, with a sheet whose Trust button was
/// disabled forever. That is the blocker.
///
/// So: the process is started and kept, its stdout is read line by line as it
/// arrives, and the write half of its stdin is held until the operator has
/// typed the digits from the other screen.
///
/// # What it refuses to do
///
/// **It does not compare the digits.** The compare belongs to the process that
/// holds the handshake (`src/peer/pair.rs`), and doing it here would mean this
/// panel deciding a security question it has no handshake to decide it
/// against. What the operator types goes down the pipe unchanged, and `tcr`
/// answers `trusted` or `refused`.
///
/// **It never ends a live run by itself except on the CLI's own deadline.**
/// ``deadline`` is `waitSeconds` off the command's first line, not a number
/// written here.
@MainActor
public final class PeerPairRun: ObservableObject {
    /// What the sheet draws. One writer: ``apply(_:)``.
    @Published public private(set) var state: PeerPairState

    /// The digits read off the OTHER Mac. Owned here so the sheet's field and
    /// the submission cannot disagree about what will be sent.
    @Published public var compare = PeerPairCompare()

    /// Set while the digits are on their way down the pipe, so Trust cannot be
    /// pressed twice into a race with itself.
    @Published public private(set) var submitting = false

    private var process: Process?
    private var input: FileHandle?
    private var deadline: Task<Void, Never>?
    /// Whatever arrived on stdout since the last newline.
    private var partial = Data()
    /// The child's own stderr, for the one case where it exits having said
    /// nothing on stdout: then this is the only thing there is to report, and
    /// reporting nothing is how a sheet ends up frozen on "waiting".
    private let errors = ErrorSink()

    /// A run that starts nothing: the harness's door in, the same shape
    /// ``PeerController/pinned(_:)`` uses. `--render-states` draws every sheet
    /// state without a subprocess of any kind.
    public init(pinned state: PeerPairState) {
        self.state = state
    }

    /// Start `tcr peer pair <address> --json` and read it.
    ///
    /// The initial state is ``PeerPairState/asking(instance:)`` with no
    /// instance yet: the knock is away the moment this process starts, and the
    /// far-side instruction appears when the command names the id it knocked
    /// under. A sheet that guessed the id would be printing an argument the
    /// other operator would type wrong.
    public init(address: String, executable: URL, environment: [String: String]? = nil) {
        self.state = .asking(instance: "")
        let process = Process()
        process.executableURL = executable
        process.arguments = PeerCommand.pairJSON(address: address)
        if let environment { process.environment = environment }
        let out = Pipe()
        let err = Pipe()
        let input = Pipe()
        process.standardOutput = out
        process.standardError = err
        process.standardInput = input
        // The write to this pipe happens when the operator presses Trust,
        // which may be after the child has given up and gone: a write to a
        // closed pipe raises `SIGPIPE`, whose default disposition terminates
        // THIS process, the menu-bar app. `TcrTool` records the whole
        // measurement; the disposition is process-wide and set once.
        TcrTool.ignoreSIGPIPE()

        let errors = self.errors
        err.fileHandleForReading.readabilityHandler = { handle in
            let data = handle.availableData
            guard !data.isEmpty else {
                handle.readabilityHandler = nil
                return
            }
            errors.append(data)
        }
        out.fileHandleForReading.readabilityHandler = { [weak self] handle in
            let data = handle.availableData
            guard !data.isEmpty else {
                handle.readabilityHandler = nil
                return
            }
            Task { @MainActor [weak self] in self?.take(data) }
        }
        process.terminationHandler = { [weak self] finished in
            let said = errors.text()
            let code = finished.terminationStatus
            Task { @MainActor [weak self] in self?.ended(exitCode: code, stderr: said) }
        }

        do {
            try process.run()
        } catch {
            // No silent fallback and no empty sheet: a `tcr` that would not
            // start is reported in its own words, in the state the sheet
            // already knows how to draw.
            self.state = .refused(error.localizedDescription)
            return
        }
        self.process = process
        self.input = input.fileHandleForWriting
    }

    /// Send the digits the operator read off the other screen, and let `tcr`
    /// do the compare.
    ///
    /// Nothing is sent until six digits are in the field
    /// (``PeerPairCompare/submission``), so a half-typed code cannot be
    /// submitted by any path, including a return key.
    public func submitComparedCode() {
        guard case .comparing = state, !submitting,
            let submission = compare.submission, let input
        else { return }
        submitting = true
        // `try?`: a child that has already exited answers `EPIPE` here, and
        // that is not this app's failure to report — its termination is, and
        // `ended(exitCode:stderr:)` reports it with the child's own words.
        try? input.write(contentsOf: Data(submission.utf8))
        try? input.close()
        self.input = nil
    }

    /// Cancel: the child is terminated and the sheet says so.
    ///
    /// Always available, in every live state, which is the other half of "a
    /// child that never exits must not hang the panel": the deadline covers
    /// the operator who walked away, and this covers the one who is still
    /// here.
    public func cancel() {
        guard state.isLive else { return }
        state = .cancelled
        stop()
    }

    /// Stop reading, close the pipe, and kill the child if it is still up.
    /// Idempotent, and safe to call from a terminal state.
    public func stop() {
        deadline?.cancel()
        deadline = nil
        try? input?.close()
        input = nil
        if let process, process.isRunning { process.terminate() }
        process = nil
    }

    deinit {
        // A sheet dismissed by any path other than the two buttons must not
        // leave a subprocess holding a handshake open for ten minutes.
        if let process, process.isRunning { process.terminate() }
    }

    // MARK: Reading

    /// Fold whatever arrived into whole lines and apply each one.
    private func take(_ data: Data) {
        partial.append(data)
        while let newline = partial.firstIndex(of: UInt8(ascii: "\n")) {
            let line = partial[partial.startIndex..<newline]
            partial = partial[partial.index(after: newline)...]
            guard let text = String(data: Data(line), encoding: .utf8),
                let event = PeerPairEvent.decode(line: text)
            else { continue }
            apply(event)
        }
    }

    /// The one place ``state`` moves on something the command said.
    private func apply(_ event: PeerPairEvent) {
        state = state.applying(event)
        guard let wait = PeerPairState.wait(from: event) else { return }
        arm(wait: wait)
    }

    /// The command's own wait, plus a margin, after which this panel stops
    /// waiting on a process that should have answered.
    ///
    /// The margin exists because both clocks start at slightly different
    /// instants and the command's own refusal is the better message: give it
    /// the chance to print one, and only then say this.
    private func arm(wait: TimeInterval) {
        deadline?.cancel()
        deadline = Task { [weak self] in
            try? await Task.sleep(nanoseconds: UInt64((wait + Self.deadlineMargin) * 1_000_000_000))
            guard !Task.isCancelled else { return }
            await MainActor.run { self?.expire(after: wait) }
        }
    }

    /// How much longer than the CLI's own wait this panel gives it.
    public static let deadlineMargin: TimeInterval = 5

    private func expire(after wait: TimeInterval) {
        guard state.isLive else { return }
        state = .refused(
            "Nobody at that Mac answered within \(Int(wait / 60)) minutes, so the request was "
                + "dropped here. It stands over there until it expires; pressing Trust again "
                + "sends a new one.")
        stop()
    }

    /// The child is gone. If the sheet is still live, it exited without
    /// answering, and the sheet says what it said rather than sitting on
    /// "waiting" forever.
    private func ended(exitCode: Int32, stderr: String) {
        deadline?.cancel()
        deadline = nil
        submitting = false
        guard state.isLive else { return }
        let said = stderr.trimmingCharacters(in: .whitespacesAndNewlines)
        state = .refused(
            said.isEmpty
                ? "tcr peer pair exited \(exitCode) without saying why."
                : said)
    }
}

/// The child's stderr, collected off the reading thread.
///
/// A small locked box rather than a captured `var`: the readability handler
/// runs on a Dispatch queue and the termination handler on another, and the
/// alternative (hopping to the main actor per chunk) would reorder a refusal
/// behind the exit that follows it.
private final class ErrorSink: @unchecked Sendable {
    private let lock = NSLock()
    private var data = Data()

    func append(_ chunk: Data) {
        lock.lock()
        data.append(chunk)
        lock.unlock()
    }

    func text() -> String {
        lock.lock()
        let copy = data
        lock.unlock()
        return String(data: copy, encoding: .utf8) ?? ""
    }
}
