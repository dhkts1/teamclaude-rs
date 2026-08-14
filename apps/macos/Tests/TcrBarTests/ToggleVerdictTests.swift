import Foundation
import XCTest

@testable import TcrBarCore

/// The toggle used to be able to do nothing at all, visibly. `tcr disable <name>`
/// rewrites the config and exits 0, a running proxy keeps serving the value it
/// read at boot, and `tcr status` prefers the live server — so the row re-polled,
/// got the OLD state back, and rendered no change and no error. These tests pin
/// the arm that closes it: exit 0 plus a fleet still reporting the old value is
/// NOT a confirmation.
///
/// The comparison is a pure function precisely so it can be tested here.
/// `ImageRenderer` cannot draw AppKit controls, so no snapshot could ever cover
/// the row that renders it.
///
/// Account names are obviously fake — this repository is public.
final class ToggleVerdictTests: XCTestCase {

    private let alice = "alice@example.com"

    /// `tcr`'s own words on the success path, from the live reproduction: the park
    /// applied to the running rotation, no config entry matched it, so it comes
    /// back on restart. Exit code 0.
    private let notSaved =
        "[tcr] warning: NOT SAVED: no config entry matches this account "
        + "— it returns to rotation on restart"

    private func fleet(_ disabled: Bool) -> PollState {
        .loaded(Fleet(accounts: [account(alice, disabled: disabled)]))
    }

    // MARK: - the durability arm: exit 0 with output is not a clean success

    /// The measured live defect this arm closes. `tcr disable alice` exited 0, the
    /// live rotation DID park the account — so the read-back confirms, and every
    /// signal the row had said success — while stderr said the change is not
    /// persisted and will be gone on restart. The row stamped `parked ✓`.
    func testAWarningOnStderrDowngradesAConfirmation() {
        // The control: the same read-back with nothing on stderr confirms cleanly.
        // Without this line the test could pass on a verdict that never confirms.
        XCTAssertEqual(
            ToggleReadback.verdict(requestedEnabled: false, account: alice, readback: fleet(true)),
            .confirmed(requestedEnabled: false)
        )

        let verdict = ToggleReadback.verdict(
            requestedEnabled: false, account: alice, readback: fleet(true), notice: notSaved)

        XCTAssertEqual(verdict, .spokeUp(notice: notSaved, about: .confirmed(requestedEnabled: false)))
        XCTAssertNotEqual(verdict, .confirmed(requestedEnabled: false))
        XCTAssertFalse(verdict.isConfirmation, "exit 0 with output is accepted, not confirmed")
        XCTAssertFalse(
            verdict.rowLabel.contains("✓"),
            "the tick is what an operator scans for; it may not appear on a park that will not survive a restart"
        )
        XCTAssertTrue(verdict.rowLabel.contains(notSaved), "tcr's own text, verbatim")
        XCTAssertEqual(
            verdict.rowLabel,
            "the fleet now reports parked — tcr said: \(notSaved)"
        )
        XCTAssertTrue(verdict.spokenLabel.contains(notSaved))
        XCTAssertFalse(
            verdict.spokenLabel.contains("confirmed"),
            "the spoken form has to be as qualified as the written one"
        )
    }

    /// The downgrade is structural — any output at all — and NOT a phrase match. A
    /// keyword list would silently pass every warning added to `tcr` after the day
    /// it was written, which is the same class of defect one level up.
    func testTheDowngradeIsStructuralNotLexical() {
        let unheardOf = "an entirely different warning nobody has written yet"
        XCTAssertEqual(
            ToggleReadback.verdict(
                requestedEnabled: true, account: alice, readback: fleet(false), notice: unheardOf),
            .spokeUp(notice: unheardOf, about: .confirmed(requestedEnabled: true))
        )
        // And an empty notice is not output: it must not manufacture the arm.
        XCTAssertEqual(
            ToggleReadback.verdict(
                requestedEnabled: true, account: alice, readback: fleet(false), notice: ""),
            .confirmed(requestedEnabled: true)
        )
    }

    /// The other warning that died at the process boundary: an older proxy with no
    /// control route, where only the config file changed. Its readback is
    /// `notHonoured` — the live proxy still reports the old value — and the notice
    /// is the only place the remedy (`tcr restart`) is named, so it has to survive
    /// beside the verdict rather than being replaced by it.
    func testANoticeAndANotHonouredReadbackBothSurvive() {
        let tooOld =
            "[tcr] WARNING: the proxy running on :3456 is too old to accept live account "
            + "control, so only the config file was changed. Run `tcr restart` ..."
        let verdict = ToggleReadback.verdict(
            requestedEnabled: false, account: alice, readback: fleet(false), notice: tooOld)
        XCTAssertEqual(verdict, .spokeUp(notice: tooOld, about: .notHonoured(requestedEnabled: false)))
        XCTAssertFalse(verdict.isConfirmation)
        XCTAssertTrue(verdict.rowLabel.contains(tooOld), "the remedy is in tcr's words or nowhere")
        XCTAssertTrue(
            verdict.rowLabel.contains("the running proxy still reports rotating"),
            "and the read-back's own finding is not lost to the notice"
        )
    }

    /// A qualified confirmation is not an acknowledgement to be blinked past: it
    /// lives long enough to read, but it still ages out, because nothing can
    /// re-verify it and this panel is a live view, not a log.
    func testANoticeOutlivesACleanConfirmationAndStillExpires() {
        let now = Date()
        let recorded = RecordedVerdict(
            verdict: .spokeUp(notice: notSaved, about: .confirmed(requestedEnabled: false)), at: now)
        XCTAssertGreaterThan(ToggleReadback.noticeLifetime, ToggleReadback.confirmationLifetime)
        XCTAssertEqual(
            ToggleReadback.visible(
                recorded, reportedDisabled: true,
                now: now.addingTimeInterval(ToggleReadback.confirmationLifetime + 1)),
            recorded.verdict,
            "still on screen when a clean ✓ would already be gone"
        )
        XCTAssertNil(
            ToggleReadback.visible(
                recorded, reportedDisabled: true,
                now: now.addingTimeInterval(ToggleReadback.noticeLifetime + 1)))
        // …and it clears the moment the fleet stops reporting what was asked, for
        // the same reason a clean confirmation does.
        XCTAssertNil(
            ToggleReadback.visible(
                recorded, reportedDisabled: false, now: now.addingTimeInterval(1)))
    }

    /// A verdict about an account that LEFT the fleet used to be immortal:
    /// `reportedDisabled` is `nil`, `nil == !requested` is never true, so the
    /// clear-on-agreement rule could not fire, and an account that came back an
    /// hour later dragged the old line onto its row. The rule is now an expiry.
    func testAnUnresolvedVerdictAboutADepartedAccountExpires() {
        let now = Date()
        for verdict: ToggleVerdict in [
            .notHonoured(requestedEnabled: false),
            .unverified(requestedEnabled: false, reason: "the fleet no longer lists this account"),
            .spokeUp(notice: notSaved, about: .notHonoured(requestedEnabled: false)),
        ] {
            let recorded = RecordedVerdict(verdict: verdict, at: now)
            // Fresh, and unresolvable: still said, because the operator who caused
            // it has to see it at all.
            XCTAssertEqual(
                ToggleReadback.visible(
                    recorded, reportedDisabled: nil, now: now.addingTimeInterval(1)),
                verdict
            )
            let expired = now.addingTimeInterval(ToggleReadback.unresolvedLifetime + 1)
            XCTAssertNil(
                ToggleReadback.visible(recorded, reportedDisabled: nil, now: expired),
                "\(verdict) about a departed account must expire, not wait for agreement"
            )
            // The drag: the account returns, rotating, long after a disable was
            // asked for. Nothing from the old attempt may reappear beside it.
            XCTAssertNil(
                ToggleReadback.visible(recorded, reportedDisabled: false, now: expired),
                "\(verdict) must not come back with the account"
            )
        }
    }

    // MARK: - the arm that matters

    /// The measured live defect, in one test: the operator asked for `disable`,
    /// `tcr` exited 0 (so we are on this code path at all), and the fleet still
    /// reports the account as rotating.
    func testExitZeroWithAFleetStillReportingTheOldStateIsNotHonoured() {
        let verdict = ToggleReadback.verdict(
            requestedEnabled: false,
            account: alice,
            readback: .loaded(Fleet(accounts: [account(alice, disabled: false)]))
        )
        XCTAssertEqual(verdict, .notHonoured(requestedEnabled: false))
        XCTAssertFalse(verdict.isConfirmation, "a proxy still reporting the old state is not a success")
        XCTAssertEqual(
            verdict.rowLabel,
            "tcr accepted it — the running proxy still reports rotating"
        )
    }

    /// The same defect in the other direction — an enable the proxy has not
    /// picked up. Both deploy orders (old proxy + new app, new app before the
    /// proxy restarts) land on this arm, so both directions have to.
    func testEnableThatTheFleetStillReportsAsParkedIsNotHonoured() {
        let verdict = ToggleReadback.verdict(
            requestedEnabled: true,
            account: alice,
            readback: .loaded(Fleet(accounts: [account(alice, disabled: true)]))
        )
        XCTAssertEqual(verdict, .notHonoured(requestedEnabled: true))
        XCTAssertEqual(verdict.rowLabel, "tcr accepted it — the running proxy still reports parked")
    }

    // MARK: - confirmed

    func testFleetReportingTheRequestedStateConfirms() {
        let parked = ToggleReadback.verdict(
            requestedEnabled: false,
            account: alice,
            readback: .loaded(Fleet(accounts: [account(alice, disabled: true)]))
        )
        XCTAssertEqual(parked, .confirmed(requestedEnabled: false))
        XCTAssertTrue(parked.isConfirmation)
        XCTAssertEqual(parked.rowLabel, "parked ✓")

        let rotating = ToggleReadback.verdict(
            requestedEnabled: true,
            account: alice,
            readback: .loaded(Fleet(accounts: [account(alice, disabled: false)]))
        )
        XCTAssertEqual(rotating, .confirmed(requestedEnabled: true))
        XCTAssertEqual(rotating.rowLabel, "rotating ✓")
    }

    /// A fleet of thirteen must be compared row-wise. Confirming from some other
    /// account's `disabled` would confirm essentially every toggle.
    func testTheComparisonUsesTheRequestedAccountNotItsNeighbours() {
        let fleet = Fleet(
            accounts: [
                account("bob@example.com", disabled: true),
                account(alice, disabled: false),
                account("carol@example.com", disabled: true),
            ]
        )
        XCTAssertEqual(
            ToggleReadback.verdict(requestedEnabled: false, account: alice, readback: .loaded(fleet)),
            .notHonoured(requestedEnabled: false)
        )
    }

    // MARK: - unverified

    /// "I could not check" must be its own arm. Folding a failed re-read into
    /// `confirmed` is the original defect with extra steps; folding it into
    /// `notHonoured` would assert a disagreement nobody observed.
    func testAFailedReadbackIsNeitherConfirmedNorNotHonoured() {
        let states: [PollState] = [
            .pending,
            .toolMissing(searched: ["/usr/local/bin"]),
            .commandFailed(exitCode: 1, message: "connection refused"),
            .undecodable(message: "dataCorrupted"),
        ]
        for state in states {
            let verdict = ToggleReadback.verdict(
                requestedEnabled: false, account: alice, readback: state)
            XCTAssertFalse(verdict.isConfirmation, "\(state) must not read as a confirmation")
            guard case .unverified = verdict else {
                return XCTFail("\(state) should be unverified, got \(verdict)")
            }
            XCTAssertTrue(verdict.rowLabel.hasPrefix("tcr accepted it — could not confirm: "))
        }
    }

    func testAnAccountMissingFromTheReadbackIsUnverified() {
        let verdict = ToggleReadback.verdict(
            requestedEnabled: false,
            account: alice,
            readback: .loaded(Fleet(accounts: [account("bob@example.com", disabled: false)]))
        )
        XCTAssertEqual(
            verdict,
            .unverified(requestedEnabled: false, reason: "the fleet no longer lists this account")
        )
    }

    // MARK: - a confirmation must not outlive its truth

    func testAFreshConfirmationIsVisibleWhileTheFleetStillAgrees() {
        let now = Date()
        let recorded = RecordedVerdict(verdict: .confirmed(requestedEnabled: false), at: now)
        XCTAssertEqual(
            ToggleReadback.visible(recorded, reportedDisabled: true, now: now.addingTimeInterval(1)),
            .confirmed(requestedEnabled: false)
        )
    }

    func testAConfirmationDisappearsWhenTheReportedStateMovesOn() {
        let now = Date()
        let recorded = RecordedVerdict(verdict: .confirmed(requestedEnabled: false), at: now)
        // The account went back into rotation. A `parked ✓` here would be a lie of
        // exactly the class this type exists to prevent.
        XCTAssertNil(
            ToggleReadback.visible(recorded, reportedDisabled: false, now: now.addingTimeInterval(1)))
        // And when the fleet has no row for it at all, there is nothing to affirm.
        XCTAssertNil(
            ToggleReadback.visible(recorded, reportedDisabled: nil, now: now.addingTimeInterval(1)))
    }

    func testAConfirmationAgesOut() {
        let now = Date()
        let recorded = RecordedVerdict(verdict: .confirmed(requestedEnabled: true), at: now)
        let late = now.addingTimeInterval(ToggleReadback.confirmationLifetime + 0.5)
        XCTAssertNil(ToggleReadback.visible(recorded, reportedDisabled: false, now: late))
        // Long enough to survive at least one 3s poll, or it would flicker away
        // before the operator saw it.
        XCTAssertGreaterThan(ToggleReadback.confirmationLifetime, 3)
    }

    /// The unresolved-disagreement arm deliberately does NOT age out: hiding it
    /// after a few seconds restores the original defect, where the row said
    /// nothing while the proxy kept serving a parked account.
    func testANotHonouredVerdictPersistsPastTheConfirmationLifetime() {
        let now = Date()
        let recorded = RecordedVerdict(verdict: .notHonoured(requestedEnabled: false), at: now)
        let muchLater = now.addingTimeInterval(ToggleReadback.confirmationLifetime * 100)
        XCTAssertEqual(
            ToggleReadback.visible(recorded, reportedDisabled: false, now: muchLater),
            .notHonoured(requestedEnabled: false)
        )
    }

    /// …and clears exactly when reality catches up — a proxy restart, say.
    func testANotHonouredVerdictClearsWhenTheFleetFinallyAgrees() {
        let now = Date()
        let recorded = RecordedVerdict(verdict: .notHonoured(requestedEnabled: false), at: now)
        XCTAssertNil(
            ToggleReadback.visible(recorded, reportedDisabled: true, now: now.addingTimeInterval(30)))
    }

    func testAnUnverifiedVerdictNeverPromotesToAConfirmation() {
        let now = Date()
        let recorded = RecordedVerdict(
            verdict: .unverified(requestedEnabled: false, reason: "no fleet read has completed"),
            at: now
        )
        // Once the fleet reports the requested state the line simply stops being
        // said; it does not turn into a `✓` nothing observed.
        XCTAssertNil(
            ToggleReadback.visible(recorded, reportedDisabled: true, now: now.addingTimeInterval(1)))
        XCTAssertEqual(
            ToggleReadback.visible(recorded, reportedDisabled: false, now: now.addingTimeInterval(1)),
            recorded.verdict
        )
    }

    func testNoRecordedVerdictRendersNothing() {
        XCTAssertNil(ToggleReadback.visible(nil, reportedDisabled: false, now: Date()))
    }

    // MARK: - every arm is spoken

    /// A verdict with no spoken form is invisible to VoiceOver, and the `✓` is
    /// punctuation. Every arm has to say something, and the three have to differ.
    func testEveryArmHasADistinctSpokenAndWrittenForm() {
        let all: [ToggleVerdict] = [
            .confirmed(requestedEnabled: false),
            .confirmed(requestedEnabled: true),
            .notHonoured(requestedEnabled: false),
            .notHonoured(requestedEnabled: true),
            .unverified(requestedEnabled: false, reason: "tcr status failed (exit 1): no output"),
            .spokeUp(notice: notSaved, about: .confirmed(requestedEnabled: false)),
            .spokeUp(notice: notSaved, about: .notHonoured(requestedEnabled: false)),
        ]
        for verdict in all {
            XCTAssertFalse(verdict.rowLabel.isEmpty)
            XCTAssertFalse(verdict.spokenLabel.isEmpty)
            XCTAssertFalse(verdict.spokenLabel.contains("✓"), "a screen reader reads ✓ as punctuation")
        }
        XCTAssertEqual(Set(all.map(\.rowLabel)).count, all.count)
        XCTAssertEqual(Set(all.map(\.spokenLabel)).count, all.count)
    }

    // MARK: - the controller records what the row will draw

    @MainActor
    func testControllerStoresTheVerdictAndANewAttemptClearsIt() async {
        let controller = AccountController()
        let now = Date()
        controller.record(
            readback: .loaded(Fleet(accounts: [account(alice, disabled: false)])),
            requestedEnabled: false,
            account: alice,
            now: now
        )
        XCTAssertEqual(
            controller.verdict(for: alice, reportedDisabled: false, now: now),
            .notHonoured(requestedEnabled: false)
        )
        // A verdict for one account must not surface on another row.
        XCTAssertNil(controller.verdict(for: "bob@example.com", reportedDisabled: false, now: now))
    }

    /// The whole path the row takes, with the durability warning in it: what the
    /// controller stores is what the row will draw, and it is not a clean `✓`.
    @MainActor
    func testControllerCarriesTheNoticeIntoWhatTheRowDraws() async {
        let controller = AccountController()
        let now = Date()
        controller.record(
            readback: .loaded(Fleet(accounts: [account(alice, disabled: true)])),
            requestedEnabled: false,
            account: alice,
            notice: notSaved,
            now: now
        )
        let drawn = controller.verdict(for: alice, reportedDisabled: true, now: now)
        XCTAssertEqual(drawn, .spokeUp(notice: notSaved, about: .confirmed(requestedEnabled: false)))
        XCTAssertEqual(drawn?.isConfirmation, false)
        XCTAssertEqual(drawn?.rowLabel.contains("✓"), false)
        XCTAssertEqual(drawn?.rowLabel.contains(notSaved), true)
        // The same call with nothing on stderr is the clean case, unchanged.
        controller.record(
            readback: .loaded(Fleet(accounts: [account(alice, disabled: true)])),
            requestedEnabled: false,
            account: alice,
            now: now
        )
        XCTAssertEqual(
            controller.verdict(for: alice, reportedDisabled: true, now: now)?.rowLabel, "parked ✓")
    }
}

/// Inert account with only the fields this comparison reads. `status` is
/// deliberately "active" even when parked — that is what live output does, and it
/// is why `disabled` is the only field compared.
private func account(_ name: String, disabled: Bool) -> Account {
    Account(
        name: name,
        priority: 1,
        status: "active",
        disabled: disabled,
        control: nil,
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
        serverDirty: false
    )
}
