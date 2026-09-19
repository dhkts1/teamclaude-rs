import Foundation

// Moved out of `PanelV4/PeersTabV4.swift`.
//
// These are pure data (no SwiftUI, no `Tok`, no `V4`), and the test target
// links `TcrBarCore` alone (`Package.swift:39-43`). While they sat in the
// executable target, every assertion about them had to read the SOURCE as
// text; here they are values a test can build and drive.

/// The tab's number and time words, in one place so two rows cannot phrase the
/// same figure differently.
public enum PeerFormat {
    /// Under 30 s reads as awake: the beacon cadence is seconds, so anything
    /// inside it is "answering now" and a dot is the honest rendering.
    public static let awakeWindowSeconds: Double = 30

    public static func sinceLastSeen(_ lastSeenMs: Int64?, now: Date) -> (phrase: String, awake: Bool) {
        guard let lastSeenMs else { return ("just now", false) }
        let seconds = max(0, now.timeIntervalSince1970 - Double(lastSeenMs) / 1000)
        return (duration(seconds), seconds < awakeWindowSeconds)
    }

    /// `2s ago` / `6m ago` / `3h ago`. One unit, never two: the row has one
    /// sub line and the precise figure is not what the operator is deciding
    /// on.
    public static func duration(_ seconds: Double) -> String { "\(span(seconds)) ago" }

    /// The same one-unit span WITHOUT `ago`: `2s`, `6m`, `3h`, `4d`.
    ///
    /// Split out of ``duration(_:)`` rather than copied, because a second
    /// caller needed the span inside a longer sentence ("blocked 3h ago ·
    /// …"). Two spellings of one rounding rule is the drift this file exists
    /// to prevent, `duration` is now this plus one word.
    public static func span(_ seconds: Double) -> String {
        let seconds = max(0, seconds)
        if seconds < 60 { return "\(Int(seconds))s" }
        if seconds < 3600 { return "\(Int(seconds / 60))m" }
        if seconds < 86400 { return "\(Int(seconds / 3600))h" }
        return "\(Int(seconds / 86400))d"
    }

    /// `a third` / `under two thirds` / `41%`. Words where a word is clearer
    /// than a number, because the sentence is prose and the bar already
    /// carries the figure.
    public static func share(_ fraction: Double) -> String {
        switch fraction {
        case ..<0.13: return "a little"
        case ..<0.4: return "a third"
        case ..<0.6: return "about half"
        case ..<0.7: return "under two thirds"
        case ..<0.9: return "most"
        default: return "nearly all"
        }
    }

    public static func requests(_ count: Int) -> String {
        count == 1 ? "One request is" : "\(count) requests are"
    }

    /// `, and may keep serving for 4 more minutes before it has to ask again`.
    /// Empty when no ttl was reported, rather than a guessed one.
    public static func ttlClause(_ seconds: Int?) -> String {
        guard let seconds, seconds > 0 else { return "" }
        let minutes = Int((Double(seconds) / 60).rounded())
        if minutes < 1 { return ", and may keep serving for under a minute before it asks again" }
        return ", and may keep serving for \(minutes) more "
            + (minutes == 1 ? "minute" : "minutes") + " before it has to ask again"
    }

    /// Megabytes, as a whole number: the byte ceiling is a 2 GB-scale figure
    /// and a decimal place on it is noise.
    public static func megabytes(_ bytes: Int64) -> String {
        "\(Int((Double(bytes) / 1_048_576).rounded()))"
    }
}

extension PeerFormat {
    /// One path's sub-line: `127.0.0.1:7751 · 19 ms · 2% lost`.
    ///
    /// # Three absences, three different words
    ///
    /// An unmeasured path says `not measured`, a measured zero loss says `no
    /// loss`, and a measured loss says its percentage. The distinction is the
    /// whole reason the wire types are optional: `0 ms · 0% lost` reads as a
    /// perfect path, and it is what a structurally-unwritten field prints.
    /// Every figure here arrives `nil` until the prober lands, so `not
    /// measured` is what this build actually draws, and it is the truthful
    /// line, not a placeholder.
    ///
    /// The kind leads rather than trails, because it changes what the endpoint
    /// IS: a ``PeerListDocument/PeerPath/Kind/via`` endpoint is another Mac's
    /// peer id and a direct one is a socket address, and an operator reading a
    /// bare id where every other row shows an address has no way to tell
    /// which. An unknown kind is drawn with its own word for the same reason,
    /// calling a relayed path direct is a claim about where the bytes went.
    public static func pathLine(_ path: PeerListDocument.PeerPath) -> String {
        var line = ""
        switch path.kind {
        case .direct: break
        case .via: line = "via "
        case .unknown(let raw): line = "\(raw) "
        }
        line += path.endpoint

        var figures: [String] = []
        if let rttMs = path.rttMs { figures.append("\(Int(rttMs.rounded())) ms") }
        if let lossPct = path.lossPct {
            figures.append(lossPct <= 0 ? "no loss" : "\(Int((lossPct * 100).rounded()))% lost")
        }
        if figures.isEmpty { figures.append("not measured") }
        return ([line] + figures).joined(separator: " · ")
    }

    /// Loss at or above this fraction reads as a path worth looking at:
    /// green under 3 per cent, amber from there.
    public static let lossWarnFraction = 0.03

    /// Every path line for one row, and the ONE case the row could not say
    /// before: no path at all.
    ///
    /// A trusted Mac with an empty `paths` array used to draw nothing, so
    /// "this build did not read the live half" and "there is no way to reach
    /// that Mac right now" looked identical, and the `asleep` pill did the
    /// work of both. Rule 5 of the mockup is what this closes: a stale reading
    /// renders as absent, and the absence is SAID rather than left blank.
    ///
    /// `names` maps a peer id to the name the operator knows, so a forwarded
    /// path reads `via loft-mini` rather than `via tcr-4b8we1r0zp`. An id with
    /// no name keeps the id: an operator can look that up, and inventing a
    /// name would be worse.
    ///
    /// The first line, the one a dial tries first, is prefixed `tried first
    /// · `. It says `tried first`, never `in use`: nothing on the wire
    /// reports which path a connection is actually using, and a label
    /// nothing measured is a lie. A row with a single path still gets the
    /// prefix, because it is still the path dialled first. Do not "improve"
    /// this into `in use` without a wire field to back it.
    public static func pathLines(
        _ paths: [PeerListDocument.PeerPath], names: [String: String] = [:]
    ) -> [PeerPathLine] {
        guard !paths.isEmpty else {
            return [PeerPathLine(text: "no path right now", tone: .absent)]
        }
        return paths.enumerated().map { index, path in
            let text = pathLine(path, names: names)
            return PeerPathLine(
                text: index == 0 ? "tried first · \(text)" : text, tone: tone(path))
        }
    }

    /// Forwarded, or losing more than the line allows: both are states worth a
    /// second look, and neither is an error. Amber, never red: a hop through a
    /// third Mac is the network doing what it was designed to do when a direct
    /// route is gone.
    private static func tone(_ path: PeerListDocument.PeerPath) -> PeerPathLine.Tone {
        if case .via = path.kind { return .warn }
        if let loss = path.lossPct, loss >= lossWarnFraction { return .warn }
        return .plain
    }

    /// ``pathLine(_:)`` with a forwarder's id resolved to its name.
    public static func pathLine(
        _ path: PeerListDocument.PeerPath, names: [String: String]
    ) -> String {
        guard case .via = path.kind, let name = names[path.endpoint] else {
            return pathLine(path)
        }
        var renamed = path
        renamed.endpoint = name
        return pathLine(renamed)
    }

    /// `studio-mac and attic-nuc`. Oxford-free, because two is the population
    /// this feature has and a list of five reads better in Settings.
    public static func list(_ names: [String]) -> String {
        switch names.count {
        case 0: return "nobody"
        case 1: return names[0]
        case 2: return "\(names[0]) and \(names[1])"
        default:
            return names.dropLast().joined(separator: ", ") + " and \(names[names.count - 1])"
        }
    }
}

/// One path's line and how it reads: ordinary, worth a look, or the absence
/// itself.
///
/// A pair rather than a bare string, because the tab has to draw the forwarded
/// and the no-path cases differently and a view re-deriving that from the words
/// would be a second place the 3 per cent rule lives.
public struct PeerPathLine: Equatable, Hashable, Sendable {
    public enum Tone: Equatable, Hashable, Sendable {
        /// Direct and healthy.
        case plain
        /// Forwarded, or losing more than ``PeerFormat/lossWarnFraction``.
        case warn
        /// There is no path right now. Not an error, and not a number.
        case absent
    }

    public let text: String
    public let tone: Tone

    public init(text: String, tone: Tone) {
        self.text = text
        self.tone = tone
    }
}
