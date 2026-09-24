import XCTest

@testable import TcrBarCore

/// `AccountName` is the one place the panel shortens an account's row name, for
/// the account card and the Sessions tab alike.
final class AccountNameTests: XCTestCase {
    func testLocalPartIsEverythingBeforeTheFirstAt() {
        XCTAssertEqual(AccountName.localPart("henry10@example.com"), "henry10")
        XCTAssertEqual(AccountName.localPart("no-at-sign"), "no-at-sign")
    }

    func testShortKeepsTheSuffixThatTellsTwoRowsOfOneLoginApart() {
        XCTAssertEqual(AccountName.short("henry@example.com"), "henry")
        XCTAssertEqual(AccountName.short("henry@example.com/research"), "henry/research")
        XCTAssertNotEqual(
            AccountName.short("henry@example.com"),
            AccountName.short("henry@example.com/research"),
            "two rows of one login must not read the same")
    }

    func testShortLeavesANameWithoutAnAtAlone() {
        XCTAssertEqual(AccountName.short("local-key"), "local-key")
        XCTAssertEqual(AccountName.short("path/like"), "path/like")
        XCTAssertEqual(AccountName.short("henry@example.com/"), "henry")
    }
}
