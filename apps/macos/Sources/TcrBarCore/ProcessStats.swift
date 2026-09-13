import Darwin
import Foundation

/// What a RUNNING Bash call is costing the box, and the one process it is
/// allowed to signal.
///
/// `docs/design/tools-tab.md` § "Actions the panel can take": a kill goes to
/// "the child shell's process group", and only to "a child whose parent is the
/// session pid and whose start matches the call". Every rule in that sentence
/// is a pure function here, over a ``ProcessSnapshot`` value the caller
/// supplies, for the reason ``MachineStats`` splits the same way: a matcher
/// that reads the live process table is a matcher no test can put a case to.
/// ``ProcessTable/read()`` is the only impure thing in this file, and nothing
/// else in it calls the kernel.
///
/// The refusals are the point. A pid is a claim about the instant it was
/// read — the process it named can exit and its number be handed to something
/// else before the next line runs — so no pid survives the poll that produced
/// it, and a click re-matches before it signals anything.
public struct ProcessSnapshot: Equatable, Sendable {
    public let pid: Int32
    /// `pbi_ppid`. The matcher's first test: the session's own `claude`
    /// process is the parent of the shell it spawned for a Bash call.
    public let parentPid: Int32
    /// `pbi_pgid` — what a kill actually signals. A shell and the pipeline it
    /// ran share one group, which is why the whole `a | b | c` dies with one
    /// `killpg` and would survive a `kill` of the shell alone.
    public let processGroup: Int32
    /// `proc_name`'s answer: `bash`, `zsh`, `rustc`.
    public let name: String
    /// `pbi_start_tvsec`, as a date. The matcher's second test.
    public let startedAt: Date
    /// `ri_user_time + ri_system_time`, in seconds. A TOTAL since the process
    /// started, never a rate: the rate is the delta of two of these over the
    /// wall time between them, which is ``RunningCallStats/cpuPercent``.
    public let cpuSeconds: Double
    /// `ri_resident_size`.
    public let residentBytes: UInt64

    public init(
        pid: Int32,
        parentPid: Int32,
        processGroup: Int32,
        name: String,
        startedAt: Date,
        cpuSeconds: Double,
        residentBytes: UInt64
    ) {
        self.pid = pid
        self.parentPid = parentPid
        self.processGroup = processGroup
        self.name = name
        self.startedAt = startedAt
        self.cpuSeconds = cpuSeconds
        self.residentBytes = residentBytes
    }
}

/// Which live process is the one a running Bash call is running in.
public enum ProcessMatch {
    /// The shells a Bash tool call can land in. Nothing else is ever matched:
    /// a call's child could be any binary at all, and matching by "whatever
    /// the session spawned last" is precisely the guess that gets the wrong
    /// process killed.
    public static let shellNames: Set<String> = ["bash", "zsh", "sh"]

    /// How far a process's start may sit from the call's own `startedMs` and
    /// still be that call. Three seconds, from the bridge's own figure: the
    /// wire stamps the call when the proxy sees it and the shell starts a
    /// moment later, but a call and a shell three seconds apart are two
    /// events, not one seen twice.
    public static let startTolerance: TimeInterval = 3

    /// The process a call is running in, or `nil`.
    ///
    /// `nil` is a first-class answer and the common one: a call whose session
    /// file carries no pid, a session whose shell has already exited, a
    /// non-Bash tool. The tab draws no stats and no kill button for it. Never
    /// a best guess — see this file's own header.
    ///
    /// When two shell children of the same parent start inside the window,
    /// the CLOSEST start wins. That is not tie-breaking for its own sake: a
    /// session running two Bash calls at once has two shells, and the one
    /// whose start is nearest this call's stamp is the only defensible read.
    public static func shellChild(
        ofSessionPid sessionPid: Int32,
        startedMs: Int64,
        in table: [ProcessSnapshot],
        tolerance: TimeInterval = startTolerance
    ) -> ProcessSnapshot? {
        let callStart = Date(timeIntervalSince1970: Double(startedMs) / 1000)
        return
            table
            .filter { $0.parentPid == sessionPid }
            .filter { shellNames.contains($0.name) }
            .filter { abs($0.startedAt.timeIntervalSince(callStart)) <= tolerance }
            .min {
                abs($0.startedAt.timeIntervalSince(callStart))
                    < abs($1.startedAt.timeIntervalSince(callStart))
            }
    }
}

/// The matched shell AND everything it spawned — the numbers the row prints.
///
/// A shell that ran `cargo build` uses almost no CPU itself; the 640% is in
/// its children. Summing the shell alone would draw "2% cpu" beside a build
/// pinning every core, which is worse than drawing nothing.
public enum ProcessTree {
    /// The root plus every descendant of it in `table`.
    ///
    /// Bounded by the table's own size rather than by recursion: a parent
    /// chain in a snapshot read across a moment of process churn can contain
    /// a cycle (a pid reused as its own ancestor's parent), and a walk that
    /// trusts it never returns.
    public static func members(of rootPid: Int32, in table: [ProcessSnapshot]) -> [ProcessSnapshot] {
        var childrenByParent: [Int32: [ProcessSnapshot]] = [:]
        for process in table {
            childrenByParent[process.parentPid, default: []].append(process)
        }
        var seen: Set<Int32> = []
        var out: [ProcessSnapshot] = []
        var queue: [ProcessSnapshot] = table.filter { $0.pid == rootPid }
        while let next = queue.popLast() {
            guard seen.insert(next.pid).inserted else { continue }
            out.append(next)
            queue.append(contentsOf: childrenByParent[next.pid] ?? [])
        }
        return out
    }

    public static func cpuSeconds(of rootPid: Int32, in table: [ProcessSnapshot]) -> Double {
        members(of: rootPid, in: table).reduce(0) { $0 + $1.cpuSeconds }
    }

    public static func residentBytes(of rootPid: Int32, in table: [ProcessSnapshot]) -> UInt64 {
        members(of: rootPid, in: table).reduce(0) { $0 + $1.residentBytes }
    }
}

/// One poll's reading for one running call.
public struct RunningCallStats: Equatable, Sendable {
    public let pid: Int32
    public let processGroup: Int32
    /// The tree's total CPU seconds at this poll — kept so the NEXT poll can
    /// take a delta against it.
    public let cpuSeconds: Double
    public let residentBytes: UInt64
    /// `nil` on the first poll for a call, which is not a zero: no delta has
    /// been taken yet, and 0% would be a claim about a process nobody has
    /// watched for any length of time. The row prints memory only.
    public let cpuPercent: Double?
    /// When this reading was taken — the wall clock the next delta divides by.
    public let readAt: Date

    public init(
        pid: Int32,
        processGroup: Int32,
        cpuSeconds: Double,
        residentBytes: UInt64,
        cpuPercent: Double?,
        readAt: Date
    ) {
        self.pid = pid
        self.processGroup = processGroup
        self.cpuSeconds = cpuSeconds
        self.residentBytes = residentBytes
        self.cpuPercent = cpuPercent
        self.readAt = readAt
    }
}

/// One running call, as the poller needs it: which session's pid to look
/// under, when the call started, and the key its reading is filed under.
public struct RunningCallKey: Equatable, Sendable {
    public let id: String
    public let sessionPid: Int32
    public let startedMs: Int64

    public init(id: String, sessionPid: Int32, startedMs: Int64) {
        self.id = id
        self.sessionPid = sessionPid
        self.startedMs = startedMs
    }
}

public enum ProcessStats {
    /// One poll: match every running call to a process, sum its tree, and
    /// take a CPU rate against the previous poll's reading for the same call.
    ///
    /// A call that matched last poll and does not match now simply drops out
    /// of the result — its shell exited, and carrying the old numbers forward
    /// would draw a live figure for a dead process.
    public static func poll(
        calls: [RunningCallKey],
        table: [ProcessSnapshot],
        previous: [String: RunningCallStats],
        now: Date,
        tolerance: TimeInterval = ProcessMatch.startTolerance
    ) -> [String: RunningCallStats] {
        var out: [String: RunningCallStats] = [:]
        for call in calls {
            guard
                let shell = ProcessMatch.shellChild(
                    ofSessionPid: call.sessionPid, startedMs: call.startedMs, in: table,
                    tolerance: tolerance)
            else { continue }
            let cpuSeconds = ProcessTree.cpuSeconds(of: shell.pid, in: table)
            let residentBytes = ProcessTree.residentBytes(of: shell.pid, in: table)
            out[call.id] = RunningCallStats(
                pid: shell.pid,
                processGroup: shell.processGroup,
                cpuSeconds: cpuSeconds,
                residentBytes: residentBytes,
                cpuPercent: cpuPercent(
                    previous: previous[call.id], pid: shell.pid, cpuSeconds: cpuSeconds, now: now),
                readAt: now)
        }
        return out
    }

    /// The rate between two readings of the same tree.
    ///
    /// `nil` — never zero — whenever there is nothing to divide: no previous
    /// reading, a previous reading of a DIFFERENT pid (the shell exited and
    /// the call re-matched to another one, so the two totals are not
    /// comparable), a non-advancing clock, or a total that went backwards.
    /// 100% is one core saturated, so a build across seven cores prints 700%,
    /// the same number `top` prints for the same work.
    static func cpuPercent(
        previous: RunningCallStats?, pid: Int32, cpuSeconds: Double, now: Date
    ) -> Double? {
        guard let previous, previous.pid == pid else { return nil }
        let wall = now.timeIntervalSince(previous.readAt)
        guard wall > 0 else { return nil }
        let used = cpuSeconds - previous.cpuSeconds
        guard used >= 0 else { return nil }
        return used / wall * 100
    }
}

/// What the row PRINTS for a reading — the strings, split from the numbers so
/// a test reads them without a live process, the same split
/// ``ToolCallLabel`` makes for the duration pill.
public enum ProcessStatsLabel {
    /// `" · 640% cpu · 2.1 GB"`, or `" · 2.1 GB"` on the first poll, or `""`
    /// for a call with no matched process at all.
    ///
    /// Leading separator included: this lands directly after the row's
    /// `· Bash`, and a caller assembling the `·` itself is a second place for
    /// the empty case to get it wrong.
    public static func clause(_ stats: RunningCallStats?) -> String {
        guard let stats else { return "" }
        let memory = " · \(gigabytes(stats.residentBytes)) GB"
        guard let percent = stats.cpuPercent else { return memory }
        return " · \(Int(percent.rounded()))% cpu\(memory)"
    }

    /// `"pid 48765 · Bash"` — the row's hover text, and its spoken value.
    /// `nil` when nothing matched: an empty tooltip is a tooltip that opens
    /// on an empty box.
    public static func hover(_ stats: RunningCallStats?, tool: String) -> String? {
        guard let stats else { return nil }
        return "pid \(stats.pid) · \(tool)"
    }

    /// The ✕'s accessibility label: `"Kill cargo test --release"`.
    ///
    /// The command, never a bare "Kill": five running rows each carrying a
    /// button labelled "Kill" is five identical controls to a screen reader,
    /// and the one thing a reader needs before pressing a destructive control
    /// is WHICH of them it is.
    public static func killLabel(subject: String) -> String { "Kill \(subject)" }

    /// The confirm's own sentence.
    ///
    /// Two facts, in the order they are needed: what will be signalled, and
    /// what happens to the session. The second is the one that decides the
    /// click — an operator who does not know whether this ends the SESSION
    /// will not press the button, and the answer (it does not) is the reason
    /// this control can be offered at all.
    ///
    /// Here in `TcrBarCore` rather than composed in the view for the reason
    /// ``ToolCallLabel``'s own doc-comment gives: a string a view builds
    /// privately is a string no assertion can read, and this one is the last
    /// thing an operator reads before a process dies.
    public static func killConfirmation(subject: String, session: String) -> String {
        "\(subject) in \(session). The session gets a tool error and continues."
    }

    /// Resident memory in binary GB with one decimal — the same unit
    /// ``MachineStats/gibibytes(_:)`` states the machine's own memory in, so
    /// the row and the line above it are quoting one number system. One
    /// decimal, unlike the machine line's whole units: a running call's
    /// footprint is routinely under a gigabyte, and "0 GB" beside a build is
    /// a reading nobody can act on.
    static func gigabytes(_ bytes: UInt64) -> String {
        String(format: "%.1f", Double(bytes) / 1_073_741_824)
    }
}

/// Signalling one running call, and the four cases that refuse to.
public enum ProcessKill {
    /// Why a kill did not happen. An error type rather than a `Bool`, because
    /// the panel logs which refusal it was and a caller that cannot tell them
    /// apart logs "kill failed" for a case that never should have been
    /// offered.
    public enum Refusal: Error, Equatable, Sendable {
        /// No process matched at click time. A pid from an earlier poll is
        /// not a fallback: see this file's header.
        case noMatch
        /// The matched process IS the session's `claude` process, or shares
        /// its group. `docs/design/tools-tab.md`: killing the session is "a
        /// terminal act" this panel does not offer, and a matcher bug that
        /// returned the session itself must not become a signal.
        case wouldSignalTheSession
        /// A group of 0 is "every process in the caller's group" and a group
        /// of 1 is launchd's. Neither is a process group this panel read off
        /// anything.
        case unsafeProcessGroup
    }

    /// The process group a click may signal, or why it may not.
    ///
    /// `matched` is deliberately the value from a match run AT CLICK TIME,
    /// not the poll's — every caller passes a fresh match, and this signature
    /// is what makes that visible at the call site.
    public static func target(matched: ProcessSnapshot?, sessionPid: Int32) -> Result<
        Int32, Refusal
    > {
        guard let matched else { return .failure(.noMatch) }
        guard matched.pid != sessionPid, matched.processGroup != sessionPid else {
            return .failure(.wouldSignalTheSession)
        }
        guard matched.processGroup > 1 else { return .failure(.unsafeProcessGroup) }
        return .success(matched.processGroup)
    }

    /// How long a process group gets to handle `SIGTERM` before `SIGKILL` —
    /// `docs/design/tools-tab.md`'s own 5 s.
    public static let escalationDelay: TimeInterval = 5

    /// `SIGTERM` the group now; `SIGKILL` it after ``escalationDelay`` if
    /// anything in it is still alive.
    ///
    /// `killpg(pgid, 0)` is the liveness probe — signal 0 checks
    /// deliverability and delivers nothing — so the second signal is sent
    /// only to a group that outlived the first. A group that already exited
    /// gives `ESRCH`, which is success here, not an error.
    @discardableResult
    public static func signal(
        processGroup: Int32,
        after: @escaping (TimeInterval, @escaping () -> Void) -> Void = { delay, work in
            DispatchQueue.global().asyncAfter(deadline: .now() + delay, execute: work)
        }
    ) -> Bool {
        guard killpg(processGroup, SIGTERM) == 0 || errno == ESRCH else { return false }
        after(escalationDelay) {
            guard killpg(processGroup, 0) == 0 else { return }
            _ = killpg(processGroup, SIGKILL)
        }
        return true
    }
}

/// The live process table. The one impure thing in this file.
public enum ProcessTable {
    /// Every process this user can see, as ``ProcessSnapshot`` values.
    ///
    /// Sized from `proc_listallpids`'s own no-buffer answer plus slack, the
    /// same way ``MachineStats/readCompileCount()`` does it and for the same
    /// reason: the count can grow between the two calls.
    ///
    /// A pid that fails either read is SKIPPED, never zero-filled — a process
    /// that exited between the listing and the read is gone, and a row of
    /// zeroes for it would be a measurement of nothing.
    public static func read() -> [ProcessSnapshot] {
        let reported = proc_listallpids(nil, 0)
        guard reported > 0 else { return [] }
        let capacity = Int(reported) + 64
        var pids = [pid_t](repeating: 0, count: capacity)
        let byteCount = Int32(capacity * MemoryLayout<pid_t>.size)
        let written = pids.withUnsafeMutableBufferPointer { buffer in
            proc_listallpids(buffer.baseAddress, byteCount)
        }
        guard written > 0 else { return [] }
        return pids.prefix(Int(written)).compactMap { pid in
            pid > 0 ? snapshot(pid: pid) : nil
        }
    }

    /// One process, or `nil` if it is gone or unreadable.
    ///
    /// The rusage read is allowed to fail on its own: a process this user
    /// cannot read resource usage for still has a parent, a group and a start
    /// time, and those are the three facts the MATCHER needs. It gets zero
    /// CPU and zero memory, which the tree sum then carries — a visible
    /// under-count, rather than the whole call losing its kill button over a
    /// permission on one child.
    static func snapshot(pid: pid_t) -> ProcessSnapshot? {
        var info = proc_bsdinfo()
        let size = Int32(MemoryLayout<proc_bsdinfo>.size)
        let read = proc_pidinfo(pid, PROC_PIDTBSDINFO, 0, &info, size)
        guard read == size else { return nil }
        var name = [CChar](repeating: 0, count: 256)
        guard proc_name(pid, &name, UInt32(name.count)) > 0 else { return nil }
        let usage = rusage(pid: pid)
        return ProcessSnapshot(
            pid: pid,
            parentPid: Int32(bitPattern: info.pbi_ppid),
            processGroup: Int32(bitPattern: info.pbi_pgid),
            name: String(cString: name),
            startedAt: Date(
                timeIntervalSince1970: Double(info.pbi_start_tvsec)
                    + Double(info.pbi_start_tvusec) / 1_000_000),
            cpuSeconds: usage?.cpuSeconds ?? 0,
            residentBytes: usage?.residentBytes ?? 0)
    }

    /// `proc_pid_rusage`'s two figures: CPU nanoseconds (user + system) and
    /// resident bytes.
    static func rusage(pid: pid_t) -> (cpuSeconds: Double, residentBytes: UInt64)? {
        var info = rusage_info_current()
        let result = withUnsafeMutablePointer(to: &info) { pointer in
            pointer.withMemoryRebound(to: rusage_info_t?.self, capacity: 1) { rebound in
                proc_pid_rusage(pid, RUSAGE_INFO_CURRENT, rebound)
            }
        }
        guard result == 0 else { return nil }
        return (
            cpuSeconds: Double(info.ri_user_time + info.ri_system_time) / 1_000_000_000,
            residentBytes: info.ri_resident_size
        )
    }
}
