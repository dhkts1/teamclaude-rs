import XCTest

@testable import TcrBarCore

/// Decision row 12's scope and row 13's end, as argv and as the sentences the
/// per-Mac sheet and the account card draw.
///
/// Every argv below is compared against the MOCKUP'S OWN LEDGER
/// (`mockups/settings-peers-short.html` scenes 62 to 64), which is the spec here, not
/// against a string this file also wrote. Where the ledger
/// prints `tcr peer lend studio-mac --scope group:work --window 7d --fraction
/// 0.20 --ttl 300 --max-inflight 2`, that is the literal asserted.
final class PeerLeaseTests: XCTestCase {

    // MARK: - Scope

    func testScopeArgumentsAreTheThreeFormsDecisionRow12Names() {
        XCTAssertEqual(LendScope.all.argument, "all")
        XCTAssertEqual(LendScope.group("work").argument, "group:work")
        XCTAssertEqual(LendScope.accounts(["alice"]).argument, "account:alice")
        XCTAssertEqual(
            LendScope.accounts(["alice", "bob"]).argument, "account:alice,bob",
            "a set of accounts is one comma-joined --scope value, per row 12's own "
                + "`account:<label>[,<label>]`")
    }

    func testScopeLabelsAreWhatThePopupShows() {
        XCTAssertEqual(LendScope.all.label, "All accounts")
        XCTAssertEqual(LendScope.group("work").label, "Group: work")
        XCTAssertEqual(LendScope.accounts(["alice"]).label, "Account: alice")
        XCTAssertEqual(LendScope.accounts(["alice", "bob"]).label, "2 accounts")
    }

    func testEveryScopeRoundTripsThroughItsArgument() {
        for scope in [
            LendScope.all, .group("work"), .accounts(["alice"]), .accounts(["alice", "bob"]),
        ] {
            XCTAssertEqual(
                LendScope.parse(scope.argument), scope,
                "\(scope.argument) did not survive the trip the peers file makes it take")
        }
    }

    /// An unknown scope is `nil`, never `all`. Widening a lease the operator
    /// narrowed is the one wrong answer here: `all` draws from every account.
    func testAnUnknownScopeRefusesRatherThanWidening() {
        XCTAssertNil(LendScope.parse("tenant:acme"))
        XCTAssertNil(LendScope.parse("group:"))
        XCTAssertNil(LendScope.parse("account:"))
        XCTAssertNil(LendScope.parse(""))
    }

    // MARK: - Argv

    func testLendArgvIsTheMockupsOwnLedgerLine() {
        XCTAssertEqual(
            PeerCommand.lend(
                peer: "studio-mac", scope: .group("work"),
                terms: LeaseTerms(window: .week, fraction: 0.20)),
            [
                "peer", "lend", "studio-mac", "--scope", "group:work",
                "--window", "7d", "--fraction", "0.20", "--ttl", "300", "--max-inflight", "2",
            ])
    }

    func testLendArgvCarriesTheSecondLeaseOnTheSameMac() {
        XCTAssertEqual(
            PeerCommand.lend(
                peer: "studio-mac", scope: .accounts(["alice"]),
                terms: LeaseTerms(window: .fableWeek, fraction: 1.0)),
            [
                "peer", "lend", "studio-mac", "--scope", "account:alice",
                "--window", "7d_oi", "--fraction", "1.00", "--ttl", "300", "--max-inflight", "2",
            ])
    }

    /// `No end` writes NO flag. An absent flag is what no end means to the
    /// CLI, and `--for 0` would be a second spelling of it.
    func testNoEndWritesNoFlagAtAll() {
        let argv = PeerCommand.lend(
            peer: "studio-mac", scope: .all, terms: .standard(for: .week), end: .none)
        XCTAssertFalse(argv.contains("--for"))
        XCTAssertFalse(argv.contains("--until"))
    }

    func testTheTwoEndsWriteTheirOwnFlag() {
        XCTAssertEqual(LeaseEnd.after("2h").flags, ["--for", "2h"])
        XCTAssertEqual(LeaseEnd.until("18:00").flags, ["--until", "18:00"])
        XCTAssertEqual(LeaseEnd.after("2h").label, "For 2h")
        XCTAssertEqual(LeaseEnd.until("18:00").label, "Until 18:00")
        XCTAssertEqual(LeaseEnd.none.label, "No end")
        XCTAssertEqual(
            PeerCommand.lend(
                peer: "studio-mac", scope: .group("work"),
                terms: LeaseTerms(window: .week, fraction: 0.20), end: .after("2h")
            ).suffix(2),
            ["--for", "2h"])
    }

    func testRevokeAndRelendNameOneLeaseEach() {
        XCTAssertEqual(
            PeerCommand.lendRevoke(peer: "studio-mac", leaseId: "ls-4b1f"),
            ["peer", "lend", "studio-mac", "--revoke", "ls-4b1f"])
        XCTAssertEqual(
            PeerCommand.lendRelend(peer: "studio-mac", leaseId: "ls-2e77"),
            ["peer", "lend", "studio-mac", "--relend", "ls-2e77"])
        XCTAssertEqual(
            PeerCommand.lendList(peer: "studio-mac"),
            ["peer", "lend", "studio-mac", "--list"])
    }

    /// The Sharing defaults sheet's first row (scene 62): the DEFAULT lease's
    /// scope. `share --scope …`, and never `share on|off` with a scope bolted
    /// on, the switch and the defaults are two different writes.
    func testShareDefaultsCarriesTheScopeAndNoSwitchWord() {
        let argv = PeerCommand.shareDefaults(
            scope: .all, terms: LeaseTerms(window: .week, fraction: 0.20))
        XCTAssertEqual(
            argv,
            [
                "peer", "share", "--scope", "all", "--window", "7d", "--fraction", "0.20",
                "--ttl", "300", "--max-inflight", "2",
            ])
        XCTAssertFalse(argv.contains("on"))
        XCTAssertFalse(argv.contains("off"))
    }

    // MARK: - Terms

    func testTheShippedDefaultsAreSimpleSurfacesOwn() {
        XCTAssertEqual(LeaseTerms.standard(for: .fableWeek).fraction, 1.0)
        XCTAssertEqual(LeaseTerms.standard(for: .week).fraction, 0.20)
        XCTAssertEqual(LeaseTerms.standard(for: .fiveHour).fraction, 0.20)
        XCTAssertEqual(LeaseTerms.standard(for: .week).ttlSeconds, 300)
        XCTAssertEqual(LeaseTerms.standard(for: .week).maxInFlight, 2)
    }

    func testTermsLabelsAreTheSheetsOwnWords() {
        XCTAssertEqual(LeaseTerms(window: .week, fraction: 0.20).label, "7-day, 20%")
        XCTAssertEqual(LeaseTerms(window: .fableWeek, fraction: 1.0).label, "Fable weekly, all")
        XCTAssertEqual(LeaseTerms(window: .fiveHour, fraction: 0.20).label, "5-hour, 20%")
    }

    // MARK: - A grant's two lines

    private func grant(
        until: Int64?, ended: Bool = false, ttl: Int = 300
    ) -> PeerLendGrant {
        PeerLendGrant(
            leaseId: "ls-4b1f", scope: .group("work"), window: .week, fraction: 0.20,
            ttlSeconds: ttl, maxInFlight: 2, until: until, ended: ended)
    }

    /// The row's two figures are DIFFERENT figures and both are on it: the
    /// stored end as a clock time, and what the operator wants to know beside
    /// it. Scene 63 reads `ends 19:00` and `in 1 h, renews every 300 s, 2 at
    /// once`.
    func testARunningGrantSaysTheStoredEndAndTheTimeLeft() {
        var calendar = Calendar(identifier: .gregorian)
        let zone = TimeZone(identifier: "UTC")
        calendar.timeZone = try! XCTUnwrap(zone)
        // 2023-11-14T18:00:00Z, so the mockup's own clock times fall out of
        // the arithmetic: a lease ending an hour later is scene 63's
        // `ends 19:00` / `in 1 h`.
        let now = Date(timeIntervalSince1970: 1_699_984_800)
        let ends = Int64(1_699_984_800 + 3600)

        XCTAssertEqual(grant(until: ends).endLabel(calendar: calendar), "ends 19:00")
        XCTAssertEqual(
            grant(until: ends).endSentence(now: now, calendar: calendar),
            "in 1h, renews every 300 s, 2 at once")
    }

    func testAGrantWithNoEndSaysOnlyItsCadence() {
        let now = Date(timeIntervalSince1970: 1_699_984_800)
        XCTAssertEqual(grant(until: nil).endLabel(), "No end")
        XCTAssertEqual(
            grant(until: nil).endSentence(now: now), "renews every 300 s, 2 at once")
    }

    /// Decision row 13 keeps an ended lease in the list rather than deleting
    /// it, and says why on the row.
    func testAnEndedGrantIsKeptAndSaysSo() {
        var calendar = Calendar(identifier: .gregorian)
        calendar.timeZone = try! XCTUnwrap(TimeZone(identifier: "UTC"))
        // 17:30 UTC, half an hour before `now`.
        let now = Date(timeIntervalSince1970: 1_699_984_800)
        let ended = grant(until: 1_699_983_000, ended: true)
        XCTAssertTrue(ended.isEnded(now: now))
        XCTAssertEqual(
            ended.endSentence(now: now, calendar: calendar),
            "ended 17:30, kept here so you can see what was lent")
    }

    /// An end that passed with no `ended` flag from the producer still reads
    /// as ended: the clock is the fallback, so a panel polling between the
    /// deadline and the lender's next write does not draw a dead lease as
    /// running.
    func testAPassedEndReadsAsEndedEvenWithoutTheProducersFlag() {
        let now = Date(timeIntervalSince1970: 1_699_984_800)
        XCTAssertTrue(grant(until: 1_699_983_000, ended: false).isEnded(now: now))
        XCTAssertFalse(grant(until: 1_699_988_400, ended: false).isEnded(now: now))
    }

    func testTheLendTagCountsRunningAndEndedSeparately() {
        let now = Date(timeIntervalSince1970: 1_699_984_800)
        let grants = [
            grant(until: 1_699_988_400), grant(until: nil), grant(until: 1_699_983_000),
        ]
        XCTAssertEqual(PeerLease.lendTag(grants, now: now), "2 running, 1 ended")
        XCTAssertEqual(PeerLease.lendTag([grant(until: nil)], now: now), "1 running")
        XCTAssertNil(
            PeerLease.lendTag([], now: now),
            "with no lease at all the sheet shows Add a lease and no tag")
    }

    // MARK: - The account card's line (scene 64)

    func testTheLentToLineIsTheMockupsOwnSentence() {
        let entries = [
            PeerLentToEntry(peer: "attic-nuc", window: .week, fraction: 0.20),
            PeerLentToEntry(peer: "studio-mac", window: .fableWeek, fraction: 1.0),
        ]
        XCTAssertEqual(
            PeerLease.lentToLine(entries),
            "Lent to attic-nuc 20 % · studio-mac Fable weekly",
            "scene 64's line, verbatim: a partial fraction is a percentage and a full one "
                + "is said in its window's words")
    }

    /// One line, not a list. Two Macs fit; a third becomes `and 1 more`,
    /// because the card says THAT the account is lent and the sheet says how
    /// much.
    func testAThirdMacBecomesAndOneMore() {
        let entries = (1...3).map {
            PeerLentToEntry(peer: "mac-\($0)", window: .week, fraction: 0.20)
        }
        XCTAssertEqual(
            PeerLease.lentToLine(entries), "Lent to mac-1 20 % · mac-2 20 % and 1 more")
    }

    /// An account inside no lease has NO line at all, which is how an
    /// operator tells the two states apart at a glance. `nil`, not an empty
    /// string: an empty line still occupies the card.
    func testAnAccountInsideNoLeaseHasNoLine() {
        XCTAssertNil(PeerLease.lentToLine([]))
    }

    // MARK: - Which account a lease belongs to

    private let lentTo: [String: [PeerLentToEntry]] = [
        "alice": [PeerLentToEntry(peer: "attic-nuc", window: .week, fraction: 0.20)],
        "[masked]": [PeerLentToEntry(peer: "studio-mac", window: .fableWeek, fraction: 1.0)],
    ]

    func testAnAccountFindsItsOwnLeases() {
        XCTAssertEqual(
            PeerLease.leases(forAccountLabel: "alice", in: lentTo).map(\.peer), ["attic-nuc"])
        XCTAssertTrue(PeerLease.leases(forAccountLabel: "bob", in: lentTo).isEmpty)
    }

    /// **The trap this lookup exists to refuse.** `tcr peer ls --json` masks
    /// any label its sanitizer rejects, and an email is rejected
    /// (`src/main.rs`'s own `peer_ls_masking_tests`), so every email-labelled
    /// account inside a lease arrives under the single key `[masked]`. That
    /// key names all of them and identifies none, and drawing its rows on the
    /// first card that asks would say a different account's allowance is being
    /// spent.
    func testAMaskedKeyIsNotHandedToAnyAccount() {
        XCTAssertTrue(
            PeerLease.leases(forAccountLabel: "[masked]", in: lentTo).isEmpty,
            "the masked key was matched to an account, which attributes one account's "
                + "lease to another")
        XCTAssertTrue(
            PeerLease.leases(forAccountLabel: "alice@example.com", in: lentTo).isEmpty,
            "an email-labelled account must not pick up the masked bucket either")
    }

    /// And the panel can SAY so, rather than an operator finding an account
    /// they know is lent with no line on its card.
    func testUnattributableLeasesAreVisibleAsAState() {
        XCTAssertTrue(PeerLease.hasUnattributableLeases(lentTo))
        XCTAssertFalse(
            PeerLease.hasUnattributableLeases([
                "alice": [PeerLentToEntry(peer: "attic-nuc", window: .week, fraction: 0.20)]
            ]))
        XCTAssertFalse(PeerLease.hasUnattributableLeases([:]))
        XCTAssertFalse(
            PeerLease.hasUnattributableLeases(["[masked]": []]),
            "an empty masked bucket is not a lease nobody can place")
    }

    // MARK: - The Defaults row's one-line readout

    /// The Sharing section's Defaults row has a label and a `Customize…`
    /// button beside it, and about 45 characters of room. The long spelling
    /// wrapped, and a wrapped value is a 64 pt row in a grouped `Form` where a
    /// single line is 40, 24 pt of a pane that had 2 pt of margin.
    ///
    /// A character count is a PROXY for one line, and it is here because the
    /// real instrument is a rendered capture: `.lineLimit(1)` in the pane is
    /// what makes a longer string truncate instead of growing the pane, and
    /// `PeersSettingsPaneRenderWiringTests` gates that. This gates the length.
    func testTheDefaultsReadoutFitsOneLine() {
        let line = PeerLease.defaultsLine
        XCTAssertLessThanOrEqual(
            line.count, 45,
            "the Defaults readout is \(line.count) characters (\(line)) and the row has "
                + "about 45 beside its label and its button")
    }

    /// It still names all three allowances and their amounts, from
    /// ``LeaseTerms/standard(for:)``, short is not the same as vague, and a
    /// readout that dropped a window would be hiding a default.
    func testTheDefaultsReadoutNamesEveryAllowanceAndItsAmount() {
        let line = PeerLease.defaultsLine
        for window in PeerLeaseWindow.allCases {
            XCTAssertTrue(
                line.contains(window.shortLabel),
                "\(window.shortLabel) is missing from the Defaults readout: \(line)")
        }
        XCTAssertTrue(line.contains("20%"), "the 0.20 fractions are not shown: \(line)")
        XCTAssertTrue(
            line.contains("full"), "Fable weekly's whole allowance is not shown: \(line)")
        XCTAssertTrue(
            line.contains("ttl 5 min"),
            "the renewal ttl is missing, and it is the one figure an operator reads as the "
                + "end of the lease: \(line)")
    }

    /// And it is computed from the terms, not typed out: a different default
    /// changes the row with no edit to it.
    func testTheDefaultsReadoutFollowsTheTerms() {
        XCTAssertTrue(
            PeerLease.defaultsLine.contains(
                "\(Int(LeaseTerms.standard(for: .week).fraction * 100))%"),
            "the readout no longer follows LeaseTerms.standard, so the row and the sheet can "
                + "print two different defaults")
    }
}
