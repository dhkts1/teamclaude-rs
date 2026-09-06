import Foundation
import XCTest

@testable import TcrBarCore

/// The "What's new" window: the launch gate, the persisted marker, where the
/// notes come from, the markdown subset, and the controller that ties them.
///
/// No network here: `FakeFetcher` stands in for GitHub. Versions and bodies
/// are made up — this repository is public.
final class WhatsNewTests: XCTestCase {

    // MARK: gate

    func testGateFreshInstallRecordsWithoutShowing() {
        XCTAssertEqual(WhatsNewGate.decide(lastSeen: nil, current: "0.2.34"), .recordOnly)
    }

    func testGateSameVersionSkips() {
        XCTAssertEqual(WhatsNewGate.decide(lastSeen: "0.2.34", current: "0.2.34"), .skip)
    }

    func testGateNewVersionShows() {
        XCTAssertEqual(
            WhatsNewGate.decide(lastSeen: "0.2.33", current: "0.2.34"), .show(version: "0.2.34"))
    }

    /// An unbundled binary or a test host has no version; that must never be
    /// the thing that opens a window.
    func testGateNilCurrentNeverShows() {
        XCTAssertEqual(WhatsNewGate.decide(lastSeen: "0.2.33", current: nil), .skip)
        XCTAssertEqual(WhatsNewGate.decide(lastSeen: nil, current: ""), .skip)
    }

    // MARK: store

    func testStoreKeyLiteralIsPinned() {
        XCTAssertEqual(WhatsNewStore.lastSeenVersionKey, "lastSeenVersion")
    }

    @MainActor
    func testStoreRoundTripsThroughDefaults() {
        let suite = "WhatsNewTests.\(UUID().uuidString)"
        let defaults = UserDefaults(suiteName: suite)!
        defer { defaults.removePersistentDomain(forName: suite) }
        XCTAssertNil(WhatsNewStore(defaults: defaults).lastSeenVersion)
        WhatsNewStore(defaults: defaults).lastSeenVersion = "0.2.34"
        XCTAssertEqual(WhatsNewStore(defaults: defaults).lastSeenVersion, "0.2.34")
    }

    @MainActor
    func testStoreWithNilDefaultsRemembersNothing() {
        let store = WhatsNewStore(defaults: nil)
        store.lastSeenVersion = "0.2.34"
        XCTAssertEqual(store.lastSeenVersion, "0.2.34", "in-memory value still moves")
        XCTAssertNil(WhatsNewStore(defaults: nil).lastSeenVersion)
    }

    // MARK: feed location

    /// The literal `build-tcrbar.sh` writes into `SUFeedURL`.
    func testFeedLocationParsesTheShippedFeedURL() {
        let loc = ReleaseFeedLocation.parse(
            feedURL: "https://github.com/dhkts1/teamclaude-rs/releases/latest/download/appcast.xml")
        XCTAssertEqual(loc, ReleaseFeedLocation(owner: "dhkts1", repo: "teamclaude-rs"))
        XCTAssertEqual(
            loc?.releaseAPIURL(tag: "v0.2.34")?.absoluteString,
            "https://api.github.com/repos/dhkts1/teamclaude-rs/releases/tags/v0.2.34")
        XCTAssertEqual(
            loc?.releasePageURL(tag: "v0.2.34")?.absoluteString,
            "https://github.com/dhkts1/teamclaude-rs/releases/tag/v0.2.34")
    }

    func testFeedLocationRejectsOtherHostsAndJunk() {
        XCTAssertNil(ReleaseFeedLocation.parse(feedURL: "https://example.com/a/b/appcast.xml"))
        XCTAssertNil(ReleaseFeedLocation.parse(feedURL: "http://github.com/a/b/appcast.xml"))
        XCTAssertNil(ReleaseFeedLocation.parse(feedURL: "https://github.com/onlyowner"))
        XCTAssertNil(ReleaseFeedLocation.parse(feedURL: "not a url"))
    }

    // MARK: markdown

    private let body = """
        ## What's new

        - `tcr login --token` checks the token first (#186).
        - Right-click → **Copy Access Token** (#186).

        A closing line.

        ## Install teamclaude-rs 0.2.34

        curl something
        """

    func testSectionExtractsUpToTheNextLevelTwoHeading() {
        let section = WhatsNewMarkdown.section("What's new", in: body)
        XCTAssertTrue(section.contains("Copy Access Token"))
        XCTAssertTrue(section.contains("A closing line."))
        XCTAssertFalse(section.contains("Install teamclaude-rs"))
        XCTAssertFalse(section.contains("curl something"))
    }

    func testSectionIsCaseInsensitiveAndFallsBackToWholeBody() {
        XCTAssertTrue(WhatsNewMarkdown.section("what's NEW", in: body).contains("Copy Access Token"))
        let plain = "just a line\n\n- and a bullet"
        XCTAssertEqual(WhatsNewMarkdown.section("What's new", in: plain), plain)
    }

    func testParseSplitsHeadingsBulletsAndParagraphs() {
        let blocks = WhatsNewMarkdown.parse(body)
        XCTAssertEqual(
            blocks,
            [
                .heading("What's new"),
                .bullet("`tcr login --token` checks the token first (#186)."),
                .bullet("Right-click → **Copy Access Token** (#186)."),
                .paragraph("A closing line."),
                .heading("Install teamclaude-rs 0.2.34"),
                .paragraph("curl something"),
            ])
    }

    func testParseJoinsWrappedParagraphLinesAndKeepsHashNumbersAsText() {
        let blocks = WhatsNewMarkdown.parse("first line\nsecond line\n\n#186 is not a heading")
        XCTAssertEqual(
            blocks, [.paragraph("first line second line"), .paragraph("#186 is not a heading")])
    }

    /// The bug the inline-only option exists for: with `.full`, a line that
    /// starts `#186` is an ATX heading and the `#186` is gone.
    func testInlineKeepsIssueNumbersBackticksAndArrows() {
        let rendered = String(WhatsNewMarkdown.inline("#186 fixed `tcr token` → done — really").characters)
        XCTAssertEqual(rendered, "#186 fixed tcr token → done — really")
    }

    func testInlineNeverThrowsOnUnbalancedMarkup() {
        let rendered = String(WhatsNewMarkdown.inline("one `unclosed **bold").characters)
        XCTAssertFalse(rendered.isEmpty)
        XCTAssertTrue(rendered.contains("unclosed"))
    }

    // MARK: controller

    private final class FakeFetcher: ReleaseFetching, @unchecked Sendable {
        var result: Result<ReleaseNotes, ReleaseFetchError>
        init(_ result: Result<ReleaseNotes, ReleaseFetchError>) { self.result = result }
        func fetchRelease(at url: URL) async throws -> ReleaseNotes { try result.get() }
    }

    private let location = ReleaseFeedLocation(owner: "alice", repo: "example")

    private func notes(_ body: String?) -> ReleaseNotes {
        ReleaseNotes(
            tagName: "v0.2.34", body: body,
            htmlURL: URL(string: "https://github.com/alice/example/releases/tag/v0.2.34"))
    }

    @MainActor
    func testInitIsInert() {
        let fetcher = FakeFetcher(.success(notes(body)))
        let c = WhatsNewController(
            store: WhatsNewStore(defaults: nil), fetcher: fetcher, location: location,
            currentVersion: "0.2.34")
        XCTAssertEqual(c.state, .idle)
        XCTAssertEqual(c.fetchCount, 0)
    }

    @MainActor
    func testFreshInstallRecordsAndDoesNotFetch() async {
        let store = WhatsNewStore(defaults: nil)
        let c = WhatsNewController(
            store: store, fetcher: FakeFetcher(.success(notes(body))), location: location,
            currentVersion: "0.2.34")
        let show = await c.checkAfterLaunch()
        XCTAssertFalse(show)
        XCTAssertEqual(store.lastSeenVersion, "0.2.34")
        XCTAssertEqual(c.fetchCount, 0)
    }

    @MainActor
    func testNewVersionLoadsNotesAndRecords() async {
        let store = WhatsNewStore(defaults: nil)
        store.lastSeenVersion = "0.2.33"
        let c = WhatsNewController(
            store: store, fetcher: FakeFetcher(.success(notes(body))), location: location,
            currentVersion: "0.2.34")
        let show = await c.checkAfterLaunch()
        XCTAssertTrue(show)
        XCTAssertEqual(store.lastSeenVersion, "0.2.34")
        guard case .notes(let version, let blocks, let page) = c.state else {
            return XCTFail("expected notes, got \(c.state)")
        }
        XCTAssertEqual(version, "0.2.34")
        XCTAssertEqual(blocks.first, .bullet("`tcr login --token` checks the token first (#186)."))
        XCTAssertEqual(page?.absoluteString, "https://github.com/alice/example/releases/tag/v0.2.34")
    }

    /// Offline at the one launch that would have shown the notes must NOT mark
    /// them seen — the next launch tries again.
    @MainActor
    func testFailedLaunchFetchLeavesVersionUnrecorded() async {
        let store = WhatsNewStore(defaults: nil)
        store.lastSeenVersion = "0.2.33"
        let c = WhatsNewController(
            store: store, fetcher: FakeFetcher(.failure(.network("offline"))), location: location,
            currentVersion: "0.2.34")
        let show = await c.checkAfterLaunch()
        XCTAssertFalse(show)
        XCTAssertEqual(store.lastSeenVersion, "0.2.33")
        guard case .failed(_, let message, _) = c.state else {
            return XCTFail("expected failed, got \(c.state)")
        }
        XCTAssertTrue(message.contains("offline"), message)
    }

    @MainActor
    func testNotFoundIsWordedAsNotYet() async {
        let c = WhatsNewController(
            store: WhatsNewStore(defaults: nil), fetcher: FakeFetcher(.failure(.notFound)),
            location: location, currentVersion: "0.2.34")
        await c.showOnDemand()
        guard case .failed(_, let message, let page) = c.state else {
            return XCTFail("expected failed, got \(c.state)")
        }
        XCTAssertEqual(message, "No release notes for v0.2.34 yet.")
        XCTAssertNotNil(page, "the human page link still shows, so they can look themselves")
    }

    @MainActor
    func testOnDemandReusesLoadedNotes() async {
        let fetcher = FakeFetcher(.success(notes(body)))
        let c = WhatsNewController(
            store: WhatsNewStore(defaults: nil), fetcher: fetcher, location: location,
            currentVersion: "0.2.34")
        await c.showOnDemand()
        await c.showOnDemand()
        XCTAssertEqual(c.fetchCount, 1)
    }

    @MainActor
    func testOnDemandRetriesAfterAFailure() async {
        let fetcher = FakeFetcher(.failure(.network("offline")))
        let c = WhatsNewController(
            store: WhatsNewStore(defaults: nil), fetcher: fetcher, location: location,
            currentVersion: "0.2.34")
        await c.showOnDemand()
        fetcher.result = .success(notes(body))
        await c.showOnDemand()
        XCTAssertEqual(c.fetchCount, 2)
        guard case .notes = c.state else { return XCTFail("expected notes, got \(c.state)") }
    }

    @MainActor
    func testEmptySectionIsReportedNotShownBlank() async {
        let c = WhatsNewController(
            store: WhatsNewStore(defaults: nil),
            fetcher: FakeFetcher(.success(notes("## Install\n\ncurl"))), location: location,
            currentVersion: "0.2.34")
        await c.showOnDemand()
        // No "What's new" heading → whole body → blocks exist, so notes show.
        guard case .notes = c.state else { return XCTFail("expected notes, got \(c.state)") }
        let empty = WhatsNewController(
            store: WhatsNewStore(defaults: nil), fetcher: FakeFetcher(.success(notes(""))),
            location: location, currentVersion: "0.2.34")
        await empty.showOnDemand()
        guard case .failed(_, let message, _) = empty.state else {
            return XCTFail("expected failed, got \(empty.state)")
        }
        XCTAssertTrue(message.contains("no notes yet"), message)
    }

    @MainActor
    func testMissingFeedLocationFailsWithoutFetching() async {
        let fetcher = FakeFetcher(.success(notes(body)))
        let c = WhatsNewController(
            store: WhatsNewStore(defaults: nil), fetcher: fetcher, location: nil,
            currentVersion: "0.2.34")
        await c.showOnDemand()
        XCTAssertEqual(fetcher === fetcher, true)
        XCTAssertEqual(c.fetchCount, 0)
        guard case .failed = c.state else { return XCTFail("expected failed, got \(c.state)") }
    }

    // MARK: GitHub client, via a stubbed URLProtocol

    final class StubProtocol: URLProtocol {
        nonisolated(unsafe) static var handler: ((URLRequest) -> (Int, [String: String], Data))?
        nonisolated(unsafe) static var lastRequest: URLRequest?

        override class func canInit(with request: URLRequest) -> Bool { true }
        override class func canonicalRequest(for request: URLRequest) -> URLRequest { request }
        override func startLoading() {
            Self.lastRequest = request
            guard let handler = Self.handler else { return }
            let (status, headers, body) = handler(request)
            let response = HTTPURLResponse(
                url: request.url!, statusCode: status, httpVersion: "HTTP/1.1",
                headerFields: headers)!
            client?.urlProtocol(self, didReceive: response, cacheStoragePolicy: .notAllowed)
            client?.urlProtocol(self, didLoad: body)
            client?.urlProtocolDidFinishLoading(self)
        }
        override func stopLoading() {}
    }

    private func stubbedClient() -> GitHubReleaseClient {
        let config = URLSessionConfiguration.ephemeral
        config.protocolClasses = [StubProtocol.self]
        return GitHubReleaseClient(session: URLSession(configuration: config), version: "0.2.34")
    }

    private let api = URL(string: "https://api.github.com/repos/alice/example/releases/tags/v0.2.34")!

    func testClientDecodesAReleaseAndSendsTheHeaders() async throws {
        StubProtocol.handler = { _ in
            (
                200, ["Content-Type": "application/json"],
                Data(
                    "{\"tag_name\":\"v0.2.34\",\"body\":\"## What's new\\n\\n- x\",\"html_url\":\"https://github.com/alice/example/releases/tag/v0.2.34\"}"
                        .utf8)
            )
        }
        let release = try await stubbedClient().fetchRelease(at: api)
        XCTAssertEqual(release.tagName, "v0.2.34")
        XCTAssertEqual(release.body, "## What's new\n\n- x")
        XCTAssertEqual(
            StubProtocol.lastRequest?.value(forHTTPHeaderField: "Accept"), "application/vnd.github+json")
        XCTAssertEqual(StubProtocol.lastRequest?.value(forHTTPHeaderField: "User-Agent"), "TcrBar/0.2.34")
    }

    func testClientMaps404AndRateLimit() async {
        StubProtocol.handler = { _ in (404, [:], Data()) }
        do {
            _ = try await stubbedClient().fetchRelease(at: api)
            XCTFail("404 must throw")
        } catch let error as ReleaseFetchError {
            XCTAssertEqual(error, .notFound)
        } catch {
            XCTFail("wrong error type: \(error)")
        }
        StubProtocol.handler = { _ in (403, ["X-RateLimit-Remaining": "0"], Data()) }
        do {
            _ = try await stubbedClient().fetchRelease(at: api)
            XCTFail("rate limit must throw")
        } catch let error as ReleaseFetchError {
            XCTAssertEqual(error, .rateLimited)
        } catch {
            XCTFail("wrong error type: \(error)")
        }
    }
}
