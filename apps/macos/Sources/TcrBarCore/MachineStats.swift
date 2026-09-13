import Darwin
import Foundation

/// What the box itself is doing — the Tools tab's machine line, "load 7.1 of
/// 14 · 48 GB of 64 used · 5 compiles · disk 210 GB free".
///
/// `docs/design/tools-tab.md` question 2: "is the box overloaded?", which an
/// operator answers today by typing `uptime` and counting `rustc` processes by
/// hand. Nothing on the panel answered it at all before this.
///
/// The reading and the RULE are separate on purpose. ``read()`` touches the
/// kernel and cannot run in a test that means anything; ``line`` and
/// ``loadTint`` are pure functions of the six numbers, so both are tested
/// against values a test writes itself.
public struct MachineStats: Equatable, Sendable {
    /// The 1-minute load average — `getloadavg`'s first sample, the same
    /// number `uptime` prints first.
    public let loadAverage: Double
    /// `ProcessInfo.processorCount`. The denominator the load is read
    /// against: 7.1 means nothing until it is 7.1 *of 14*.
    public let cores: Int
    /// Active + wired + compressed pages. Not "total minus free": on macOS
    /// the free list is nearly empty by design, and a line built on it reads
    /// 63 of 64 on an idle machine.
    public let memoryUsedBytes: UInt64
    public let memoryTotalBytes: UInt64
    /// Live compiler processes — `rustc`, `swift-frontend`, `clang`,
    /// `cc1plus`, `ld`. The load average says the box is busy; this says
    /// whether it is busy with a build, which is the half that clears on its
    /// own.
    public let compiles: Int
    public let diskFreeBytes: UInt64

    public init(
        loadAverage: Double,
        cores: Int,
        memoryUsedBytes: UInt64,
        memoryTotalBytes: UInt64,
        compiles: Int,
        diskFreeBytes: UInt64
    ) {
        self.loadAverage = loadAverage
        self.cores = cores
        self.memoryUsedBytes = memoryUsedBytes
        self.memoryTotalBytes = memoryTotalBytes
        self.compiles = compiles
        self.diskFreeBytes = diskFreeBytes
    }

    /// How hard the box is being pushed, by Gil's own dispatch rule
    /// (`docs/design/tools-tab.md`: "load above 2x cores means one lane, not
    /// three"). A band, not a colour: the view picks the colour, and the
    /// threshold stays where a test can read it.
    public enum LoadTint: String, Equatable, Sendable {
        /// Below one load unit per core — nothing to say.
        case calm
        /// At or above cores, up to twice cores. Amber.
        case busy
        /// Above twice cores. Red: one more lane is the wrong call.
        case overloaded
    }

    /// The boundaries are INCLUSIVE at the bottom of each band: exactly `1x`
    /// cores is already `busy` and exactly `2x` is still `busy`, so
    /// `overloaded` means strictly past the rule's own number rather than at
    /// it.
    public var loadTint: LoadTint {
        guard cores > 0 else { return .calm }
        let perCore = loadAverage / Double(cores)
        if perCore < 1 { return .calm }
        if perCore <= 2 { return .busy }
        return .overloaded
    }

    /// "load 7.1 of 14 · 48 GB of 64 used · 5 compiles · disk 210 GB free".
    public var line: String { "\(loadClause) · \(restClause)" }

    /// The load clause alone — the one part a view TINTS, split out here so
    /// the tint and the words it colours can never drift apart.
    public var loadClause: String { "load \(String(format: "%.1f", loadAverage)) of \(cores)" }

    /// Everything after the load — memory, compiles, disk. Never tinted: one
    /// coloured clause on a line is a signal, three are decoration.
    public var restClause: String {
        let compileNoun = compiles == 1 ? "compile" : "compiles"
        return
            "\(Self.gibibytes(memoryUsedBytes)) GB of \(Self.gibibytes(memoryTotalBytes)) used"
            + " · \(compiles) \(compileNoun) · disk \(Self.gigabytes(diskFreeBytes)) GB free"
    }

    /// Memory in binary GB, which is the unit a Mac's own "64 GB" memory
    /// figure is stated in: 68,719,476,736 bytes IS the 64 in "64 GB of RAM".
    static func gibibytes(_ bytes: UInt64) -> Int {
        Int((Double(bytes) / 1_073_741_824).rounded())
    }

    /// Disk in decimal GB, which is the unit the disk's own capacity and
    /// every Finder readout are stated in. Two units in one line is not a
    /// slip: it is what each of the two figures means where the operator
    /// already reads it.
    static func gigabytes(_ bytes: UInt64) -> Int {
        Int((Double(bytes) / 1_000_000_000).rounded())
    }
}

extension MachineStats {
    /// Process names that count as a compile. `ld` is here because a release
    /// link is the part of a Rust build that pins one core for a minute with
    /// nothing else on the box moving.
    static let compilerProcessNames: Set<String> = [
        "rustc", "swift-frontend", "clang", "cc1plus", "ld",
    ]

    /// Read the box, now. Every figure comes from a kernel call — no
    /// subprocess, because this runs on the panel's poll and `ps` costs a
    /// fork every few seconds for a line of text.
    ///
    /// A reading that fails is a zero in that one field, never a `nil` whole:
    /// `getloadavg` returning -1 does not make the memory figure unknown.
    public static func read() -> MachineStats {
        MachineStats(
            loadAverage: readLoadAverage(),
            cores: ProcessInfo.processInfo.processorCount,
            memoryUsedBytes: readMemoryUsed(),
            memoryTotalBytes: ProcessInfo.processInfo.physicalMemory,
            compiles: readCompileCount(),
            diskFreeBytes: readDiskFree()
        )
    }

    static func readLoadAverage() -> Double {
        var samples = [Double](repeating: 0, count: 3)
        guard getloadavg(&samples, 3) > 0 else { return 0 }
        return samples[0]
    }

    /// Active + wired + compressed pages, per `vm_stat`'s own arithmetic.
    static func readMemoryUsed() -> UInt64 {
        var stats = vm_statistics64_data_t()
        var count = mach_msg_type_number_t(
            MemoryLayout<vm_statistics64_data_t>.size / MemoryLayout<integer_t>.size)
        let result = withUnsafeMutablePointer(to: &stats) { pointer in
            pointer.withMemoryRebound(to: integer_t.self, capacity: Int(count)) { rebound in
                host_statistics64(mach_host_self(), HOST_VM_INFO64, rebound, &count)
            }
        }
        guard result == KERN_SUCCESS else { return 0 }
        let pageSize = UInt64(vm_kernel_page_size)
        let pages =
            UInt64(stats.active_count) + UInt64(stats.wire_count)
            + UInt64(stats.compressor_page_count)
        return pages * pageSize
    }

    /// `statfs` on `/`. `f_bavail` rather than `f_bfree`: the blocks a
    /// non-root process may actually have, which is the number that decides
    /// whether the next worktree fits.
    static func readDiskFree() -> UInt64 {
        var buffer = statfs()
        guard statfs("/", &buffer) == 0 else { return 0 }
        return UInt64(buffer.f_bavail) * UInt64(buffer.f_bsize)
    }

    /// Every live pid, named, counted against ``compilerProcessNames``.
    ///
    /// `proc_listallpids` reports more pids than it can return when the
    /// buffer is short, so the buffer is sized from its own no-buffer answer
    /// plus slack for processes that start between the two calls.
    static func readCompileCount() -> Int {
        let reported = proc_listallpids(nil, 0)
        guard reported > 0 else { return 0 }
        let capacity = Int(reported) + 64
        var pids = [pid_t](repeating: 0, count: capacity)
        let byteCount = Int32(capacity * MemoryLayout<pid_t>.size)
        let written = pids.withUnsafeMutableBufferPointer { buffer in
            proc_listallpids(buffer.baseAddress, byteCount)
        }
        guard written > 0 else { return 0 }
        var count = 0
        var name = [CChar](repeating: 0, count: 256)
        for pid in pids.prefix(Int(written)) where pid > 0 {
            let length = proc_name(pid, &name, UInt32(name.count))
            guard length > 0 else { continue }
            if compilerProcessNames.contains(String(cString: name)) { count += 1 }
        }
        return count
    }
}
