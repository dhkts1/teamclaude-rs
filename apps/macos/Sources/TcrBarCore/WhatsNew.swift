import Combine
import Foundation

// MARK: - Store

/// The version whose release notes the operator has already been shown,
/// persisted in `UserDefaults` — the one fact the "What's new" window's
/// launch gate reads.
///
/// ## The key string is load-bearing
///
/// Same warning as ``LaunchPreference``: renaming ``lastSeenVersionKey`` fails
/// nothing and silently forgets what the operator has seen, so the next launch
/// re-shows notes they already closed. `WhatsNewTests` pins the literal.
///
/// `defaults` is optional, mirroring `AwakeController.harness()`: `nil` means
/// "remember nothing", which is what a render or probe run must use — a
/// harness that wrote to the operator's real preferences would change what
/// the real app shows next launch.
@MainActor
public final class WhatsNewStore: ObservableObject {
    /// The `UserDefaults` key. Do not change it. See the type's doc-comment.
    public static let lastSeenVersionKey = "lastSeenVersion"

    private let defaults: UserDefaults?

    /// Written through on every change. `nil` is "never shown anything".
    @Published public var lastSeenVersion: String? {
        didSet { defaults?.set(lastSeenVersion, forKey: Self.lastSeenVersionKey) }
    }

    public init(defaults: UserDefaults? = .standard) {
        self.defaults = defaults
        // Property initialisation in `init` does not fire `didSet`.
        self.lastSeenVersion = defaults?.string(forKey: Self.lastSeenVersionKey)
    }
}

// MARK: - Gate

/// The pure decision behind "show the notes on this launch?". Split out so the
/// four cases are testable without a bundle, a network or a window.
public enum WhatsNewGate {
    public enum Decision: Equatable, Sendable {
        /// Nothing was ever recorded — a fresh install. Record the current
        /// version and show nothing: there is no "what changed" for a first run.
        case recordOnly
        /// The operator has already seen this version's notes.
        case skip
        /// The app is running a version the operator has not seen notes for.
        case show(version: String)
    }

    /// - Parameters:
    ///   - lastSeen: ``WhatsNewStore/lastSeenVersion``.
    ///   - current: `AppBuild.shortVersion`. `nil` (an unbundled binary, a test
    ///     host) never shows — there is no version to fetch notes for, and a
    ///     harness must not be the thing that opens a window.
    public static func decide(lastSeen: String?, current: String?) -> Decision {
        guard let current, !current.isEmpty else { return .skip }
        guard let lastSeen else { return .recordOnly }
        return lastSeen == current ? .skip : .show(version: current)
    }
}

// MARK: - Where the releases live

/// `owner`/`repo` on GitHub, derived from the bundle's Sparkle feed URL rather
/// than written down a second time. `build-tcrbar.sh` puts
/// `https://github.com/<owner>/<repo>/releases/latest/download/appcast.xml`
/// into `SUFeedURL`; the release API the notes come from is the same
/// repository, so the feed URL is the one source and this type just reads it.
public struct ReleaseFeedLocation: Equatable, Sendable {
    public let owner: String
    public let repo: String

    public init(owner: String, repo: String) {
        self.owner = owner
        self.repo = repo
    }

    /// `nil` for anything that is not `https://github.com/<owner>/<repo>/…`.
    public static func parse(feedURL: String) -> ReleaseFeedLocation? {
        guard let url = URL(string: feedURL),
            url.scheme == "https",
            url.host?.lowercased() == "github.com"
        else { return nil }
        let parts = url.pathComponents.filter { $0 != "/" }
        guard parts.count >= 2, !parts[0].isEmpty, !parts[1].isEmpty else { return nil }
        return ReleaseFeedLocation(owner: parts[0], repo: parts[1])
    }

    /// Reads `SUFeedURL` from `bundle`. `nil` when the key is absent — an
    /// unbundled `swift build` binary — or does not parse.
    public static func from(bundle: Bundle) -> ReleaseFeedLocation? {
        guard let feed = bundle.object(forInfoDictionaryKey: "SUFeedURL") as? String else {
            return nil
        }
        return parse(feedURL: feed)
    }

    /// The API endpoint for one release by tag.
    public func releaseAPIURL(tag: String) -> URL? {
        URL(string: "https://api.github.com/repos/\(owner)/\(repo)/releases/tags/\(tag)")
    }

    /// The page a human reads for the same release.
    public func releasePageURL(tag: String) -> URL? {
        URL(string: "https://github.com/\(owner)/\(repo)/releases/tag/\(tag)")
    }
}

// MARK: - Markdown, the small subset a release body uses

/// A release body reduced to the three block shapes the notes use. Inline
/// styling (backticks, bold) stays in the text and is rendered by
/// `AttributedString(markdown:)` per block — WITH `.inlineOnlyPreservingWhitespace`,
/// never `.full`: under `.full` a bullet remainder like `#186 fixed …` is an
/// ATX heading and the `#186` disappears from the rendered line.
public enum WhatsNewMarkdown {
    public enum Block: Equatable, Sendable {
        case heading(String)
        case bullet(String)
        case paragraph(String)
    }

    /// The body of the `## <heading>` section — up to the next `## ` line —
    /// or the whole body when no such heading exists. Case-insensitive on the
    /// heading text, since "What's new" and "What's New" have both been typed.
    public static func section(_ heading: String, in body: String) -> String {
        let lines = body.components(separatedBy: .newlines)
        let wanted = heading.lowercased()
        guard
            let start = lines.firstIndex(where: {
                headingText(of: $0)?.lowercased() == wanted
            })
        else { return body }
        var out: [String] = []
        for line in lines[(start + 1)...] {
            if headingText(of: line) != nil { break }
            out.append(line)
        }
        return out.joined(separator: "\n")
    }

    /// `## Title` → `Title`; anything else → `nil`. Only level-2 headings are
    /// section boundaries in a release body (level 3 is a sub-heading inside).
    static func headingText(of line: String) -> String? {
        let trimmed = line.trimmingCharacters(in: .whitespaces)
        guard trimmed.hasPrefix("## ") else { return nil }
        return String(trimmed.dropFirst(3)).trimmingCharacters(in: .whitespaces)
    }

    /// Split into blocks. Blank lines are dropped; consecutive non-blank,
    /// non-bullet, non-heading lines join into one paragraph, as markdown
    /// renders them.
    public static func parse(_ markdown: String) -> [Block] {
        var blocks: [Block] = []
        var paragraph: [String] = []
        func flush() {
            if !paragraph.isEmpty {
                blocks.append(.paragraph(paragraph.joined(separator: " ")))
                paragraph.removeAll()
            }
        }
        for raw in markdown.components(separatedBy: .newlines) {
            let line = raw.trimmingCharacters(in: .whitespaces)
            if line.isEmpty {
                flush()
            } else if let heading = anyHeadingText(of: line) {
                flush()
                blocks.append(.heading(heading))
            } else if let bullet = bulletText(of: line) {
                flush()
                blocks.append(.bullet(bullet))
            } else {
                paragraph.append(line)
            }
        }
        flush()
        return blocks
    }

    /// `#`…`######` followed by a space. `#186` has no space, so it is text.
    static func anyHeadingText(of line: String) -> String? {
        let hashes = line.prefix { $0 == "#" }
        guard (1...6).contains(hashes.count) else { return nil }
        let rest = line.dropFirst(hashes.count)
        guard rest.first == " " else { return nil }
        return rest.trimmingCharacters(in: .whitespaces)
    }

    static func bulletText(of line: String) -> String? {
        for marker in ["- ", "* ", "+ "] where line.hasPrefix(marker) {
            return String(line.dropFirst(marker.count)).trimmingCharacters(in: .whitespaces)
        }
        return nil
    }

    /// Inline markdown for ONE block's text, whitespace preserved, or the raw
    /// text when the parser rejects it (an unmatched backtick must not blank
    /// the whole window).
    public static func inline(_ text: String) -> AttributedString {
        let options = AttributedString.MarkdownParsingOptions(
            interpretedSyntax: .inlineOnlyPreservingWhitespace)
        return (try? AttributedString(markdown: text, options: options)) ?? AttributedString(text)
    }
}

// MARK: - GitHub

/// The fields of a GitHub release this feature reads.
public struct ReleaseNotes: Decodable, Equatable, Sendable {
    public let tagName: String
    public let body: String?
    public let htmlURL: URL?

    enum CodingKeys: String, CodingKey {
        case tagName = "tag_name"
        case body
        case htmlURL = "html_url"
    }

    public init(tagName: String, body: String?, htmlURL: URL?) {
        self.tagName = tagName
        self.body = body
        self.htmlURL = htmlURL
    }
}

public enum ReleaseFetchError: Error, Equatable, Sendable {
    /// No release for that tag — the notes are not written yet, or this is a
    /// build nobody released. Expected right after a tag push, not a fault.
    case notFound
    /// GitHub's unauthenticated limit (60/hour per address) is spent.
    case rateLimited
    /// No answer at all: offline, DNS, timeout.
    case network(String)
    case unexpectedStatus(Int)
}

/// Abstracts the one network call so the controller is testable with a fake.
public protocol ReleaseFetching: Sendable {
    func fetchRelease(at url: URL) async throws -> ReleaseNotes
}

/// The real thing: one `GET` to the releases API with the headers GitHub asks
/// for. `no_proxy` is not needed — this is a GUI process with no proxy env,
/// and api.github.com is not a host `tcr` intercepts.
public struct GitHubReleaseClient: ReleaseFetching {
    private let session: URLSession
    private let userAgent: String

    /// - Parameter version: goes into `User-Agent: TcrBar/<version>` so GitHub
    ///   can tell this client apart from an anonymous script.
    public init(session: URLSession = .shared, version: String?) {
        self.session = session
        self.userAgent = "TcrBar/\(version ?? "unknown")"
    }

    public func fetchRelease(at url: URL) async throws -> ReleaseNotes {
        var request = URLRequest(url: url)
        request.timeoutInterval = 15
        request.setValue("application/vnd.github+json", forHTTPHeaderField: "Accept")
        request.setValue(userAgent, forHTTPHeaderField: "User-Agent")
        let data: Data
        let response: URLResponse
        do {
            (data, response) = try await session.data(for: request)
        } catch {
            throw ReleaseFetchError.network(error.localizedDescription)
        }
        guard let http = response as? HTTPURLResponse else {
            throw ReleaseFetchError.network("no HTTP response")
        }
        switch http.statusCode {
        case 200:
            do {
                return try JSONDecoder().decode(ReleaseNotes.self, from: data)
            } catch {
                throw ReleaseFetchError.unexpectedStatus(200)
            }
        case 404:
            throw ReleaseFetchError.notFound
        case 403, 429:
            if http.value(forHTTPHeaderField: "X-RateLimit-Remaining") == "0" || http.statusCode == 429 {
                throw ReleaseFetchError.rateLimited
            }
            throw ReleaseFetchError.unexpectedStatus(http.statusCode)
        default:
            throw ReleaseFetchError.unexpectedStatus(http.statusCode)
        }
    }
}

// MARK: - Controller

/// What the window shows, and the two ways it gets opened.
///
/// `init` does nothing — no fetch, no timer. The launch gate is
/// ``checkAfterLaunch()``, called exactly once from
/// `applicationDidFinishLaunching`, which neither the render harness nor the
/// shell probe ever reaches. That, not a flag, is what keeps a probe run from
/// touching the network or opening a window; `WhatsNewTests` pins the
/// zero-side-effect `init`.
@MainActor
public final class WhatsNewController: ObservableObject {
    public enum State: Equatable, Sendable {
        case idle
        case loading(version: String)
        case notes(version: String, blocks: [WhatsNewMarkdown.Block], page: URL?)
        case failed(version: String, message: String, page: URL?)
    }

    /// The section of the release body the window shows.
    public static let sectionHeading = "What's new"

    @Published public private(set) var state: State = .idle

    private let store: WhatsNewStore
    private let fetcher: ReleaseFetching
    private let location: ReleaseFeedLocation?
    private let currentVersion: String?
    /// How many fetches this controller has made — read by tests to prove the
    /// cache and the inert `init`.
    public private(set) var fetchCount = 0

    public init(
        store: WhatsNewStore,
        fetcher: ReleaseFetching,
        location: ReleaseFeedLocation?,
        currentVersion: String?
    ) {
        self.store = store
        self.fetcher = fetcher
        self.location = location
        self.currentVersion = currentVersion
    }

    /// The window's title.
    public var title: String {
        "What's new in TcrBar \(currentVersion ?? "")".trimmingCharacters(in: .whitespaces)
    }

    /// The launch gate. Returns `true` when the caller should present the
    /// window. Records the version only once notes were actually loaded: a
    /// launch that could not reach GitHub stays unrecorded and tries again
    /// next launch, rather than marking notes as seen that nobody saw.
    public func checkAfterLaunch() async -> Bool {
        switch WhatsNewGate.decide(lastSeen: store.lastSeenVersion, current: currentVersion) {
        case .skip:
            return false
        case .recordOnly:
            store.lastSeenVersion = currentVersion
            return false
        case .show(let version):
            await load(version: version)
            guard case .notes = state else { return false }
            store.lastSeenVersion = version
            return true
        }
    }

    /// The footer button and the right-click item. Reuses notes already
    /// loaded for this version; otherwise fetches, and a failure is shown in
    /// the window rather than swallowed. Records the version either way — the
    /// operator asked, so nothing needs to pop up unbidden later.
    public func showOnDemand() async {
        guard let version = currentVersion, !version.isEmpty else {
            state = .failed(version: "", message: "This build has no version to look up.", page: nil)
            return
        }
        if case .notes(let loaded, _, _) = state, loaded == version {
            store.lastSeenVersion = version
            return
        }
        await load(version: version)
        store.lastSeenVersion = version
    }

    private func load(version: String) async {
        let tag = "v\(version)"
        let page = location?.releasePageURL(tag: tag)
        guard let location, let api = location.releaseAPIURL(tag: tag) else {
            state = .failed(
                version: version,
                message: "This build does not say where its releases live (no feed URL).",
                page: nil)
            return
        }
        state = .loading(version: version)
        fetchCount += 1
        do {
            let release = try await fetcher.fetchRelease(at: api)
            let body = release.body ?? ""
            let blocks = WhatsNewMarkdown.parse(
                WhatsNewMarkdown.section(Self.sectionHeading, in: body))
            if blocks.isEmpty {
                state = .failed(
                    version: version, message: "The \(tag) release has no notes yet.",
                    page: release.htmlURL ?? page)
            } else {
                state = .notes(version: version, blocks: blocks, page: release.htmlURL ?? page)
            }
        } catch let error as ReleaseFetchError {
            state = .failed(version: version, message: Self.message(for: error, tag: tag), page: page)
        } catch {
            state = .failed(version: version, message: error.localizedDescription, page: page)
        }
    }

    static func message(for error: ReleaseFetchError, tag: String) -> String {
        switch error {
        case .notFound: return "No release notes for \(tag) yet."
        case .rateLimited: return "GitHub is rate-limiting this address — try again in an hour."
        case .network(let detail): return "Couldn't reach GitHub: \(detail)"
        case .unexpectedStatus(let code): return "GitHub answered HTTP \(code)."
        }
    }
}
