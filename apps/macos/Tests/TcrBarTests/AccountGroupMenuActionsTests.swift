import XCTest

@testable import TcrBarCore

/// `Account.groupMenuActions` — the row-level right-click menu that changes
/// group membership (bridge: `docs/plans/group-tags-bridge.md`; originally
/// added by `docs/plans/stacked-cards-bridge.md`, whose own deck-card
/// summary-line tests this file used to carry were deleted along with
/// `GroupSection`/`GroupDeckCard`). Account names are obviously fake — this
/// repository is public.
final class AccountGroupMenuActionsTests: XCTestCase {

    /// An account in two groups lists a "Remove from <group>" for each,
    /// plus "Remove from all groups", plus "Add to group…" last.
    func testAccountInTwoGroupsListsBothRemovalsPlusRemoveAllPlusAdd() {
        let account = menuAccount("a@example.com", groups: ["dev", "codereview"])
        XCTAssertEqual(
            account.groupMenuActions,
            [
                .remove(group: "codereview"),
                .remove(group: "dev"),
                .removeAll,
                .addToGroup,
            ]
        )
    }

    /// A single membership gets its own removal action but NOT
    /// "Remove from all groups" — a second control doing the exact same
    /// thing as the first is noise, not a convenience.
    func testAccountInOneGroupOffersNoRemoveAll() {
        let account = menuAccount("a@example.com", groups: ["dev"])
        XCTAssertEqual(account.groupMenuActions, [.remove(group: "dev"), .addToGroup])
    }

    /// An account in no group offers only "Add to group…" — the missing
    /// affordance the bridge exists to add, with nothing to remove.
    func testAccountInNoGroupOffersOnlyAddToGroup() {
        let account = menuAccount("a@example.com", groups: nil)
        XCTAssertEqual(account.groupMenuActions, [.addToGroup])
    }

    /// Same for an explicitly empty (not nil) groups array — the wire's
    /// other spelling of "no membership".
    func testAccountWithEmptyGroupsArrayOffersOnlyAddToGroup() {
        let account = menuAccount("a@example.com", groups: [])
        XCTAssertEqual(account.groupMenuActions, [.addToGroup])
    }
}

/// Hand-built accounts with the fields this file's assertions touch.
private func menuAccount(
    _ name: String,
    groups: [String]?
) -> Account {
    Account(
        name: name,
        priority: 1,
        status: "active",
        disabled: false,
        quota: 0,
        quotaState: .ok,
        fiveHour: 0,
        sevenDay: 0,
        sevenDayOi: 0,
        held: [],
        requests: 0,
        inputTokens: 0,
        outputTokens: 0,
        cacheReadTokens: 0,
        cacheHitRatio: nil,
        probeStatus: .ok,
        probeError: nil,
        lastStreamError: nil,
        streamErrorCount: 0,
        source: .live,
        serverSha: "abc1234",
        serverDirty: false,
        groups: groups
    )
}
