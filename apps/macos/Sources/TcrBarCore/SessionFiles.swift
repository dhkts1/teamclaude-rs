import Foundation

/// The join for the Sessions tab: `~/.claude/sessions/*.json`, one file per
/// live Claude Code process, keyed by `sessionId` to line up with
/// ``Session/sessionId`` from the proxy's own wire (`panel-tabs-bridge.md`
/// "The join for the Sessions tab").
///
/// One field of this file (`bridgeSessionId`) is read by the sessions-wire
/// lane server-side, not by this build — see `panel-tabs.md`'s "Not
/// verified" note on a resumed session's id. Nothing here re-derives that.
public struct SessionFile: Decodable, Equatable, Sendable {
    public let sessionId: String
    public let cwd: String?
    public let name: String?
    /// `"busy"`, `"idle"` or `"waiting"`, verbatim from the file — never
    /// pattern-matched here beyond ``SessionActivity``'s own mapping.
    public let status: String?
    public let updatedAt: Int64?

    public init(
        sessionId: String,
        cwd: String? = nil,
        name: String? = nil,
        status: String? = nil,
        updatedAt: Int64? = nil
    ) {
        self.sessionId = sessionId
        self.cwd = cwd
        self.name = name
        self.status = status
        self.updatedAt = updatedAt
    }
}

/// Reads every session file on disk, keyed by ``SessionFile/sessionId``.
public enum SessionFiles {
    /// `~/.claude/sessions`.
    public static var defaultDirectory: URL {
        FileManager.default.homeDirectoryForCurrentUser
            .appendingPathComponent(".claude", isDirectory: true)
            .appendingPathComponent("sessions", isDirectory: true)
    }

    /// One poll's worth of session files, tolerant of everything that can go
    /// wrong reading them — a missing directory (most machines running
    /// `tcr` have never run Claude Code's CLI at all) and a file that is not
    /// valid JSON or does not carry ``SessionFile``'s shape (a partial write
    /// mid-save is routine for a file another process rewrites continuously).
    /// Both skip rather than fail the whole read, per `panel-tabs-bridge.md`:
    /// "Tolerate a missing directory and unparsable files: skip, never fail
    /// the poll."
    public static func read(directory: URL = defaultDirectory) -> [String: SessionFile] {
        guard
            let urls = try? FileManager.default.contentsOfDirectory(
                at: directory, includingPropertiesForKeys: nil)
        else { return [:] }

        let decoder = JSONDecoder()
        var out: [String: SessionFile] = [:]
        for url in urls where url.pathExtension == "json" {
            guard
                let data = try? Data(contentsOf: url),
                let file = try? decoder.decode(SessionFile.self, from: data)
            else { continue }
            out[file.sessionId] = file
        }
        return out
    }
}

/// A live/idle/waiting reading for a joined session — the file side of
/// ``JoinedSession/activity``, kept as its own type rather than a raw string
/// the same way ``Account/AccountHealth`` keeps `status` from leaking its raw
/// spelling into every call site. `.unknown` is the join-has-no-file case,
/// not a fourth server-reported state.
public enum SessionActivity: Equatable, Sendable {
    case busy, idle, waiting, unknown

    init(rawStatus: String?) {
        switch rawStatus?.lowercased() {
        case "busy": self = .busy
        case "idle": self = .idle
        case "waiting": self = .waiting
        default: self = .unknown
        }
    }
}

/// A wire ``Session`` joined to its ``SessionFile``, if any — the row the
/// Sessions and Tools tabs actually draw.
///
/// `file == nil` is routine, not an error: `panel-tabs-bridge.md` names it
/// directly ("A wire session with no file shows its id's first 8 chars and no
/// project") for the case where the harness on the other end of the proxy is
/// not Claude Code at all, or its session file has already aged out.
public struct JoinedSession: Identifiable, Equatable, Sendable {
    public let session: Session
    public let file: SessionFile?

    public init(session: Session, file: SessionFile?) {
        self.session = session
        self.file = file
    }

    public var id: String { session.sessionId }

    /// The file's `name`, else the session id's first 8 characters — never
    /// the bare full id, which is wider than this panel and no more readable.
    public var displayName: String {
        if let name = file?.name, !name.isEmpty { return name }
        return String(session.sessionId.prefix(8))
    }

    /// `cwd`'s last path component, `nil` when there is no file to read a
    /// `cwd` from at all. Never guessed from the session id.
    public var project: String? {
        guard let cwd = file?.cwd, !cwd.isEmpty else { return nil }
        return (cwd as NSString).lastPathComponent
    }

    public var activity: SessionActivity { SessionActivity(rawStatus: file?.status) }

    public var lastSeenAt: Date { Date(timeIntervalSince1970: Double(session.lastSeenMs) / 1000) }

    /// `"3m"`, `"2h"`, `"4d"` — the same tiering ``HeldWindow/duration(minutes:)``
    /// already gives a countdown, read backwards for an elapsed span instead.
    /// `now` is a required parameter, not a default, so a test stays
    /// deterministic the way every other clock-reading function here is.
    public func ageLabel(now: Date) -> String {
        let elapsedSeconds = max(0, now.timeIntervalSince(lastSeenAt))
        let minutes = Int((elapsedSeconds / 60).rounded())
        return minutes < 1 ? "now" : HeldWindow.duration(minutes: minutes)
    }
}

/// Joins the wire's sessions to the files on disk. A file with no matching
/// wire session is dropped — `panel-tabs-bridge.md`: "it never went through
/// the proxy" — so this always returns exactly `sessions.count` rows.
public enum SessionJoin {
    public static func join(sessions: [Session], files: [String: SessionFile]) -> [JoinedSession] {
        sessions.map { JoinedSession(session: $0, file: files[$0.sessionId]) }
    }
}
