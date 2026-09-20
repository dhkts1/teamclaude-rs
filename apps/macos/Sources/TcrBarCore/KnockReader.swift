import Combine
import Foundation

/// Runs `tcr peer pending --json` on a timer and publishes the Macs waiting on
/// an answer.
///
/// # Why this is its own reader and not a key on the status poll
///
/// The menu bar has to know a Mac is asking **while the panel is closed**, and
/// nothing it already reads can say so. ``StatusPoller`` runs `tcr status
/// --json`, which is a bare array of accounts and has clients that depend on
/// being exactly that (`src/main.rs`'s `run_peer_status` says so in as many
/// words), and ``Fleet/decode(_:)`` throws on anything that is not that array,
/// so there is no key on that document for a knock to ride. The Peers tab's own
/// `PeerController` does read pending rows, and it starts on the tab's
/// `onAppear` and stops on its `onDisappear`, which is precisely the case this
/// is for.
///
/// So: one more subprocess every three seconds, owned by the shell beside the
/// poller and alive for the same span.
///
/// # Why `peer pending` and not `peer status`
///
/// `pending` reads the peer STATE FILE and never opens a socket. `tcr peer
/// status --json` asks the running proxy, so a proxy that is down would answer
/// "no knocks": a silence that looks exactly like nobody asking, on the one
/// surface whose whole job is to say that somebody is.
///
/// # What a failed read is
///
/// Not an empty list. A spawn that fails, a non-zero exit, output this build
/// cannot decode: each leaves ``knocks`` at its last good value and sets
/// ``lastReadFailed``, because "this Mac cannot currently tell" and "nobody is
/// asking" are different facts and the second one is the one that draws
/// nothing. A row that will not decode costs that row and never the read, the
/// rule ``Fleet/decode(_:)`` already follows for accounts.
@MainActor
public final class KnockReader: ObservableObject {
    /// Every Mac waiting on an answer, as of the last read that succeeded.
    @Published public private(set) var knocks: [PeerKnock] = []
    /// Whether the last read failed. A reader of ``knocks`` that wants to
    /// distinguish "nobody is asking" from "this build could not ask" reads
    /// this beside it.
    @Published public private(set) var lastReadFailed = false
    /// Whether any read has completed at all. The notifier needs it: the
    /// FIRST read after launch is adopted silently, and "empty because
    /// nothing has run yet" must not be mistaken for that first read.
    @Published public private(set) var hasRead = false

    public let interval: TimeInterval
    private var task: Task<Void, Never>?

    public init(interval: TimeInterval = StatusPoller.defaultInterval) {
        self.interval = interval
    }

    /// A reader pinned to a set of knocks, for deterministic rendering and for
    /// tests. It never starts a timer and never spawns anything, the same seam
    /// ``StatusPoller/init(pinnedState:lastPollAt:)`` gives the panel.
    public init(pinnedKnocks: [PeerKnock]) {
        self.interval = StatusPoller.defaultInterval
        self.knocks = pinnedKnocks
        self.hasRead = true
    }

    deinit { task?.cancel() }

    public func start() {
        guard task == nil else { return }
        task = Task { [weak self] in
            guard let self else { return }
            while !Task.isCancelled {
                await self.readOnce()
                try? await Task.sleep(nanoseconds: UInt64(self.interval * 1_000_000_000))
            }
        }
    }

    public func stop() {
        task?.cancel()
        task = nil
    }

    /// One read, published. Returns what it published so a caller that reads
    /// *in order to act on it* compares against its own read rather than
    /// against whatever a later tick left behind.
    @discardableResult
    public func readOnce() async -> [PeerKnock] {
        switch await Self.fetch() {
        case .read(let rows):
            knocks = rows
            lastReadFailed = false
        case .failed:
            lastReadFailed = true
        }
        hasRead = true
        return knocks
    }

    /// What one read produced: rows, or a failure that leaves the last good
    /// rows standing. A typed answer rather than `[PeerKnock]?`, so a caller
    /// cannot read an absence as an empty list by writing `?? []`.
    enum Read: Equatable {
        case read([PeerKnock])
        case failed
    }

    /// Always off the main actor: `Process`'s own wait spins the run loop, and
    /// a three-second timer that blocked the main thread would be felt in
    /// every animation on the panel.
    nonisolated static func fetch() async -> Read {
        switch TcrTool.resolve() {
        case .failure:
            return .failed
        case .success(let executable):
            return await Task.detached(priority: .utility) {
                do {
                    return decode(
                        try TcrTool.run(
                            executable: executable, arguments: PeerCommand.pending))
                } catch {
                    return .failed
                }
            }.value
        }
    }

    /// `{"pending": [...], "muted": [...], "banned": [...]}`, the document
    /// `tcr peer pending --json` prints, down to the rows this reader is for.
    ///
    /// Row at a time, for the reason ``Fleet/decode(_:)`` gives: one knock a
    /// newer `tcr` writes in a shape this build cannot read must cost that
    /// knock and not the other Mac asking beside it.
    nonisolated static func decode(_ output: TcrTool.Output) -> Read {
        guard output.exitCode == 0 else { return .failed }
        guard
            let top = try? JSONSerialization.jsonObject(with: output.stdout),
            let object = top as? [String: Any]
        else { return .failed }
        // An ABSENT `pending` key is a document this build does not understand
        // and never an empty queue: an older `tcr` whose verb prints something
        // else entirely would otherwise read as "nobody is asking".
        guard let rows = object["pending"] as? [Any] else { return .failed }

        let decoder = JSONDecoder()
        var knocks: [PeerKnock] = []
        for row in rows {
            guard JSONSerialization.isValidJSONObject(row),
                let data = try? JSONSerialization.data(withJSONObject: row),
                let knock = try? decoder.decode(PeerKnock.self, from: data)
            else { continue }
            knocks.append(knock)
        }
        return .read(knocks)
    }
}
