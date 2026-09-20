import Foundation

/// A record of every `tcr` this process spawned, written by the child itself.
///
/// ## Why the child writes the line
///
/// The question this exists to answer is whether a part of the app keeps
/// shelling out when nothing is on screen, and the honest instrument for that
/// is the process table, not a counter somebody remembered to increment. A
/// counter inside the app would have to be placed at the one call site the
/// author already suspects, and would therefore agree with them.
///
/// So the executable is substituted instead: ``stub`` is a script that appends
/// its own argv to ``logURL`` and exits, and ``TcrTool/resolve(environment:defaults:home:fileManager:bundle:)``
/// hands it to every caller through the environment override it already
/// supports. Nothing about how the app spawns changes, no call site is touched,
/// and a spawn from a path nobody thought of is recorded exactly like the rest.
/// It also means a measuring run reaches no proxy and changes no file.
///
/// One `printf` per child, under the pipe-buffer size, appended `O_APPEND`: two
/// children finishing at once each land one whole line.
public struct SpawnLog {
    /// Where the stub and the log live. The caller owns its lifetime.
    public let directory: URL
    public let stub: URL
    public let logURL: URL

    /// Writes the stub and an empty log.
    ///
    /// The stub answers the read verbs with the smallest document each decoder
    /// accepts, so the panel under measurement draws its ordinary empty state
    /// rather than an error card. A verb it has no answer for prints nothing
    /// and exits 0, which every caller here already treats as an empty read.
    public init(directory: URL, fileManager: FileManager = .default) throws {
        self.directory = directory
        self.stub = directory.appendingPathComponent("tcr")
        self.logURL = directory.appendingPathComponent("spawns.log")
        try fileManager.createDirectory(at: directory, withIntermediateDirectories: true)
        try Data().write(to: logURL)
        try Self.script(log: logURL.path).write(to: stub, atomically: true, encoding: .utf8)
        try fileManager.setAttributes([.posixPermissions: 0o755], ofItemAtPath: stub.path)
    }

    /// The one account the stub reports is obviously fake and is the smallest
    /// row `tcr status --json` can carry: the panel draws its tabs only over a
    /// fleet with something in it, so a stub that reported none would measure a
    /// panel stuck on its empty state rather than the tab under question.
    static let account = """
        {"source":"live","serverSha":"abc1234","serverDirty":false,\
        "name":"alice@example.com","priority":1,"status":"active","disabled":false,\
        "quota":0.1,"quotaState":"ok","fiveHour":0.1,"sevenDay":0.1,"sevenDayOi":0.0,\
        "held":[],"requests":2,"inputTokens":64,"outputTokens":8,"cacheReadTokens":1,\
        "cacheHitRatio":null,"probeStatus":"ok","probeError":null,\
        "lastStreamError":null,"streamErrorCount":0}
        """

    static func script(log: String) -> String {
        """
        #!/bin/sh
        printf '%s %s\\n' "$(date +%s)" "$*" >> '\(log)'
        case "$1 $2" in
        'status --json') printf '%s\\n' '[\(account)]' ;;
        'sessions --json') printf '%s\\n' '{"supported":true,"sessions":[]}' ;;
        'peer ls') printf '%s\\n' '{"supported":true,"finding":false,"sharing":false,"peers":[]}' ;;
        'peer pending') printf '%s\\n' '{"knocks":[]}' ;;
        esac
        exit 0

        """
    }

    /// One spawn: when the child ran, to the second, and the argv it ran with.
    public struct Entry: Equatable, Sendable {
        public let at: Int
        public let argv: String
    }

    /// Every spawn recorded so far, oldest first.
    ///
    /// A line with no readable stamp keeps its argv and lands at second zero
    /// rather than being dropped: a count is what this file is for, and a lost
    /// line would quietly make the answer smaller.
    public func entries() -> [Entry] {
        guard let text = try? String(contentsOf: logURL, encoding: .utf8) else { return [] }
        return text.split(separator: "\n", omittingEmptySubsequences: true).map { line in
            let halves = line.split(separator: " ", maxSplits: 1, omittingEmptySubsequences: false)
            guard halves.count == 2, let at = Int(halves[0]) else {
                return Entry(at: 0, argv: String(line))
            }
            return Entry(at: at, argv: String(halves[1]))
        }
    }

    /// Every argv recorded so far, oldest first.
    public func argvLines() -> [String] { entries().map(\.argv) }

    /// How many spawns have been recorded. Taken before a measured window and
    /// handed back to ``argvLines(after:)`` afterwards, so a window counts what
    /// happened inside it and not what the run did to get there.
    public func mark() -> Int { argvLines().count }

    public func argvLines(after mark: Int) -> [String] { entries(after: mark).map(\.argv) }

    public func entries(after mark: Int) -> [Entry] {
        let all = entries()
        guard mark < all.count else { return [] }
        return Array(all[mark...])
    }

    /// The verb an argv line names: the words before the first flag, at most
    /// two of them.
    ///
    /// Two, because that is the depth at which these verbs differ and the depth
    /// at which the answer is readable: `peer ls` and `peer status` are two
    /// different reads on a cadence, while `peer ls --json` and a future
    /// `peer ls --json --wide` are one. A line with a flag first, or no line at
    /// all, is its own bucket rather than being dropped: a spawn nobody can
    /// name is exactly the spawn this measurement is looking for.
    public static func verb(ofArgv line: String) -> String {
        let words = line.split(separator: " ", omittingEmptySubsequences: true).map(String.init)
        let named = words.prefix { !$0.hasPrefix("-") }.prefix(2)
        return named.isEmpty ? "(no verb)" : named.joined(separator: " ")
    }

    /// One row per verb, most spawns first, then alphabetically so a tie does
    /// not reorder between two runs of the same measurement.
    public static func counts(ofArgv lines: [String]) -> [(verb: String, count: Int)] {
        var tally: [String: Int] = [:]
        for line in lines { tally[verb(ofArgv: line), default: 0] += 1 }
        return tally.map { (verb: $0.key, count: $0.value) }
            .sorted { $0.count == $1.count ? $0.verb < $1.verb : $0.count > $1.count }
    }
}
