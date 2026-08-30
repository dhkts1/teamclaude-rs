import XCTest

@testable import TcrBarCore

/// ``AppBuild``'s formatting rule, exercised through the pure entry points.
///
/// The `Bundle.main`-reading properties are deliberately NOT tested here: under
/// `swift test` the main bundle is the test runner, so asserting on them would
/// pin the harness's own Info.plist rather than TcrBar's. What is worth pinning
/// is the rule that decides what reaches the panel — above all that an absent
/// version draws nothing instead of a plausible-looking placeholder.
final class AppBuildTests: XCTestCase {
    func testTheLabelNamesTheAppSoTwoHashesInOneFooterAreTellableApart() {
        XCTAssertEqual(AppBuild.label(version: "0.2.29", sha: "9582244"), "TcrBar 0.2.29 · 9582244")
    }

    func testAMissingShaLeavesTheVersionStandingAlone() {
        XCTAssertEqual(AppBuild.label(version: "0.2.29", sha: nil), "TcrBar 0.2.29")
    }

    /// The whole reason these are optionals. A binary run outside its bundle
    /// has no Info.plist and therefore no version; the panel then says nothing,
    /// because a fabricated build number in the one place a reader goes to
    /// check what they are running is worse than an empty corner.
    func testNoVersionDrawsNothingRatherThanAPlaceholder() {
        XCTAssertNil(AppBuild.label(version: nil, sha: "9582244"))
        XCTAssertNil(AppBuild.label(version: nil, sha: nil))
    }

    func testTheBuildNumberIsHoverDetailNotALine() {
        XCTAssertEqual(AppBuild.buildDetail(buildNumber: "258"), "build 258")
        XCTAssertNil(AppBuild.buildDetail(buildNumber: nil))
    }
}
