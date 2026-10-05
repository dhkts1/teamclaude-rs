import Foundation
import XCTest

@testable import TcrBarCore

/// The Sessions row's `cache 59m` clause. Every case pins the clock (`now`) and the last
/// request time in whole milliseconds, so the wording and the colour thresholds are exact.
final class CacheCountdownTests: XCTestCase {
    private let now = Date(timeIntervalSince1970: 1_700_000_000)
    private var nowMs: Int64 { 1_700_000_000_000 }

    /// A request `secondsAgo` seconds before `now`, against a tier of `ttl` seconds.
    private func reading(secondsAgo: Int64, ttl: Int?) -> CacheCountdown.Reading? {
        CacheCountdown.reading(lastSeenMs: nowMs - secondsAgo * 1000, ttlSecs: ttl, now: now)
    }

    func testAnHourTierJustUsedReadsFiftyNineMinutes() {
        let r = reading(secondsAgo: 30, ttl: 3600)
        XCTAssertEqual(r?.text, "cache 59m")
        XCTAssertEqual(r?.tone, .normal)
    }

    func testUnderAMinuteLeftReadsSeconds() {
        let r = reading(secondsAgo: 3560, ttl: 3600)
        XCTAssertEqual(r?.text, "cache 40s")
        XCTAssertEqual(r?.tone, .danger)
    }

    func testAtZeroAndPastZeroReadsCold() {
        XCTAssertEqual(reading(secondsAgo: 3600, ttl: 3600)?.text, "cache cold")
        XCTAssertEqual(reading(secondsAgo: 3600, ttl: 3600)?.tone, .cold)
        XCTAssertEqual(reading(secondsAgo: 9000, ttl: 3600)?.text, "cache cold")
        XCTAssertEqual(reading(secondsAgo: 301, ttl: 300)?.text, "cache cold")
    }

    func testAMissingTierDrawsNoClauseAtAll() {
        XCTAssertNil(reading(secondsAgo: 10, ttl: nil))
    }

    /// The five-minute tier counts from its own 300 s, not from an hour.
    func testAFiveMinuteTierCountsFromThreeHundredSeconds() {
        XCTAssertEqual(reading(secondsAgo: 60, ttl: 300)?.text, "cache 4m")
    }

    /// Normal above a tenth of the tier, near in the last tenth, danger in the last minute.
    func testToneBandsFollowTheTier() {
        // 1 h tier: a tenth is 360 s, the danger line is 60 s.
        XCTAssertEqual(reading(secondsAgo: 3600 - 361, ttl: 3600)?.tone, .normal)
        XCTAssertEqual(reading(secondsAgo: 3600 - 360, ttl: 3600)?.tone, .near)
        XCTAssertEqual(reading(secondsAgo: 3600 - 61, ttl: 3600)?.tone, .near)
        XCTAssertEqual(reading(secondsAgo: 3600 - 60, ttl: 3600)?.tone, .danger)
        XCTAssertEqual(reading(secondsAgo: 3600 - 1, ttl: 3600)?.tone, .danger)
    }
}
