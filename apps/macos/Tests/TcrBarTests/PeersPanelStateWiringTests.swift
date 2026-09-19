import XCTest

@testable import TcrBarCore

/// The eight gaps the UI walkthrough found between the design rulings
/// and the drawn panel.
///
/// # Why half of these read the source
///
/// The words, the argv and the row shapes are values in `TcrBarCore` and are
/// driven directly below. The rest are facts about a SwiftUI view's structure
/// which control a row carries, what a sheet says when it has no digits,
/// which count feeds a tab badge, and the test target links `TcrBarCore`
/// alone (`Package.swift:39-43`), so it cannot build `PeersTabV4` or
/// `FleetView` at all. Those are asserted against the source that draws them,
/// the shape `PeersPanelViewWiringTests` and `PeersSettingsPaneRenderWiringTests` already use,
/// with anchor-slicing so one assertion cannot match an identical line
/// elsewhere in a 1,600-line view. The instrument for the drawn result is
/// `--render-states` / `--render-settings` plus a person's eye.
final class PeersPanelStateWiringTests: XCTestCase {

    // MARK: - Item 1: the found row's third state

    /// The waiting row and the waiting sheet are one fragment, said twice.
    func testTheWaitingWordsAreOneFragment() {
        XCTAssertEqual(
            PeerAdmission.waitingLine(name: "studio-mac"), "waiting for studio-mac to accept")
        XCTAssertEqual(
            PeerAdmission.waitingTitle(name: "studio-mac"), "Waiting for studio-mac to accept",
            "the sheet's title is not the row's own fragment as a sentence, so the two can "
                + "phrase one state two ways")
    }

    /// And they are one fragment in the SOURCE, not two literals that happen
    /// to agree today.
    ///
    /// Measured: replacing the title's body with its own literal left the
    /// equality test above green, because two identical strings are equal.
    /// Only the source says which of the two is the single spelling.
    func testTheSheetTitleIsBuiltFromTheRowFragment() throws {
        let words = try source("apps/macos/Sources/TcrBarCore/PeerAdmission.swift")
        XCTAssertTrue(
            words.contains("sentence(waitingLine(name: name))"),
            "the sheet's title is a second literal again rather than the row's fragment as a "
                + "sentence, so the two can drift apart on the next edit")
    }

    /// A Mac name is not re-cased on the way into a sentence.
    ///
    /// `capitalized` would have written `Studio-Mac`, which is a different
    /// Mac's name as far as the operator reading it is concerned.
    func testTheSentenceHelperTouchesOnlyTheFirstCharacter() {
        XCTAssertEqual(
            PeerAdmission.waitingTitle(name: "attic-nuc lab"),
            "Waiting for attic-nuc lab to accept")
    }

    /// Nothing in the waiting copy claims the other screen is showing digits.
    ///
    /// Decision row 10: the six digits are phase 2 and appear only after
    /// somebody there presses Accept. The sheet used to assert they were
    /// already on screen, which is the blocker this item closes.
    func testTheWaitingCopyPromisesNoDigitsYet() {
        for sentence in [
            PeerAdmission.waitingLine(name: "studio-mac"),
            PeerAdmission.waitingSentence,
            PeerAdmission.cancelWaitingHelp,
        ] {
            XCTAssertFalse(
                sentence.contains("is showing"),
                "waiting copy claims the other Mac is already showing something: \(sentence)")
            XCTAssertFalse(
                sentence.contains("\u{2014}"), "an em dash is in the waiting copy: \(sentence)")
        }
        XCTAssertTrue(
            PeerAdmission.waitingSentence.contains("six digits appear on both screens then"),
            "the waiting sentence no longer says WHEN the digits arrive, which is the one "
                + "thing an operator staring at a waiting row wants")
    }

    /// Trust sends the knock and records that it went, in one call.
    func testTheTrustControlKnocksAndRemembers() throws {
        let tab = try source("apps/macos/Sources/TcrBar/PanelV4/PeersTabV4.swift")
        let card = try slice(
            tab, from: "private func peerCard(", to: "/// The freshness readout AND")
        XCTAssertTrue(
            card.contains("controller.knock(address: row.id, arguments: arguments)"),
            "the Trust control no longer goes through the one call that both sends the knock "
                + "and records it, so the row can go back to being byte-identical after a "
                + "press")
        XCTAssertTrue(
            card.contains("controller.stopWaiting(address: address)"),
            "the waiting row lost its Cancel")
        XCTAssertTrue(
            card.contains("help: PeerAdmission.cancelWaitingHelp"),
            "Cancel writes its own help again rather than the one gated above")
    }

    /// The waiting overlay is applied over a snapshot kept as it was read, so
    /// Cancel can undo it.
    func testTheWaitingOverlayIsReversible() throws {
        let tab = try source("apps/macos/Sources/TcrBar/PanelV4/PeersTabV4.swift")
        XCTAssertTrue(
            tab.contains("snapshot = PeersSnapshotBuilder.waiting(readSnapshot, knocked: knocked)"),
            "the published snapshot is no longer the read one plus the waiting overlay: a "
                + "transform applied in place cannot be undone by Cancel")
        XCTAssertTrue(
            tab.contains("guard case .found = row.trust, knocked.contains(row.id) else"),
            "the overlay no longer restricts itself to FOUND rows this panel knocked at")
    }

    // MARK: - Item 2: the sheet with no digits

    /// The waiting sheet says what is true and draws no digit block.
    func testTheTrustSheetWithNoCodeClaimsNoDigits() throws {
        let tab = try source("apps/macos/Sources/TcrBar/PanelV4/PeersTabV4.swift")
        let sheet = try slice(tab, from: "struct PeerTrustSheet: View {", to: "/// The tab's pill")
        XCTAssertTrue(
            sheet.contains("PeerAdmission.waitingTitle(name: peerName)")
                && sheet.contains("PeerAdmission.waitingSentence"),
            "the waiting sheet no longer uses the gated waiting words, so it can claim "
                + "something about the other screen again")
        XCTAssertTrue(
            sheet.contains("if let code {"),
            "the digit block is drawn unconditionally again, so a sheet with no digits draws "
                + "a placeholder where a number belongs")
        XCTAssertFalse(
            sheet.contains("······"),
            "the six middle dots are back: that is a password field's glyph and it reads as "
                + "digits present but masked")
        XCTAssertFalse(
            sheet.contains("is showing the same six digits. If they match, press Trust on "),
            "the compare sentence is unconditional again, which is the blocker: it asserts "
                + "the other Mac is showing digits while the request sits unanswered")
        XCTAssertTrue(
            sheet.contains("enabled: code != nil"),
            "Trust is no longer gated on there being a code to compare")
    }

    // MARK: - Item 3: a knock raises a badge on the tab

    /// The Peers tab's badge is the producer's own pending count.
    func testTheTabStripBadgesAPairingRequest() throws {
        let fleet = try source("apps/macos/Sources/TcrBar/FleetView.swift")
        let badges = try slice(
            fleet, from: "private var v4Badges: [PanelTab: Int] {", to: "@ViewBuilder")
        XCTAssertTrue(
            badges.contains("out[.peers] = peers.snapshot.pendingCount"),
            "the tab strip does not badge a pairing request, so a knock arrives, expires "
                + "after ten minutes, and an operator on another tab is never told")
        XCTAssertFalse(
            badges.contains("peers.snapshot.pending.count"),
            "the badge counts the ROWS this build was sent rather than the requests the "
                + "producer counted, and those two disagree on purpose when rows are held back")
        let peersLine = try XCTUnwrap(badges.range(of: "out[.peers]")).lowerBound
        let fleetGuard = try XCTUnwrap(badges.range(of: "guard case .loaded")).lowerBound
        XCTAssertTrue(
            peersLine < fleetGuard,
            "the peers badge is back inside the fleet guard, so a knock raises no badge while "
                + "the fleet read is down")
    }

    // MARK: - Item 4: Add a lease opens a sheet and writes nothing

    /// A new draft is the shipped default at the widest scope, and it is only
    /// a draft: the argv exists, nothing has run it.
    func testANewLeaseDraftIsTheShippedDefault() {
        let draft = LeaseDraft.new(peer: "studio-mac")
        XCTAssertTrue(draft.isNew)
        XCTAssertEqual(draft.scope, .all)
        XCTAssertEqual(draft.terms, LeaseTerms.standard(for: .week))
        XCTAssertEqual(draft.end, .none)
        XCTAssertEqual(
            draft.arguments,
            [
                "peer", "lend", "studio-mac", "--scope", "all", "--window", "7d",
                "--fraction", "0.20", "--ttl", "300", "--max-inflight", "2",
            ],
            "Save no longer writes the whole lease in one call")
    }

    /// Editing an existing lease starts from THAT lease, not from the default.
    func testEditingALeaseStartsFromTheLeaseItself() {
        let grant = PeerLendGrant(
            leaseId: "ls-4b1f", scope: .group("work"), window: .week, fraction: 0.30,
            ttlSeconds: 600, maxInFlight: 3)
        let draft = LeaseDraft(editing: grant, peer: "studio-mac")
        XCTAssertFalse(draft.isNew)
        XCTAssertEqual(draft.leaseId, "ls-4b1f")
        XCTAssertEqual(draft.scope, .group("work"))
        XCTAssertEqual(draft.terms.fraction, 0.30)
        XCTAssertEqual(draft.terms.ttlSeconds, 600)
        XCTAssertTrue(
            draft.arguments.contains("0.30"),
            "a fraction that is not one of the three shipped defaults cannot be expressed, "
                + "which is the blocker: 30 % of the work group was unreachable at any depth")
    }

    /// The two values the drawn controls can reach and the CLI must not be
    /// sent are refused, with the reason on screen.
    func testADraftRefusesAZeroShareAndAnEndAlreadyPassed() {
        let noon = Date(timeIntervalSince1970: 1_786_000_000)
        var draft = LeaseDraft.new(peer: "studio-mac")
        XCTAssertNil(draft.refusal(now: noon), "the shipped default is refused")

        draft.terms.fraction = 0
        XCTAssertNotNil(draft.refusal(now: noon), "a share of zero is written as a lease")

        draft = LeaseDraft.new(peer: "studio-mac")
        let calendar = Calendar(identifier: .gregorian)
        let hour = calendar.component(.hour, from: noon)
        draft.end = .until(String(format: "%02d:00", max(0, hour - 1)))
        XCTAssertNotNil(
            draft.refusal(now: noon, calendar: calendar),
            "an end that has already passed today is sent, so the lease stops the moment it "
                + "starts")
        draft.end = .until(String(format: "%02d:00", min(23, hour + 1)))
        XCTAssertNil(
            draft.refusal(now: noon, calendar: calendar),
            "an end still ahead today is refused")
    }

    /// The clock guard answers about the clock, not about the CLI's grammar.
    func testTheClockGuardIgnoresWhatItCannotRead() {
        let noon = Date(timeIntervalSince1970: 1_786_000_000)
        let calendar = Calendar(identifier: .gregorian)
        XCTAssertFalse(
            PeerLease.clockHasPassed("tomorrow", now: noon, calendar: calendar),
            "a spelling this panel cannot parse is refused on the panel's behalf rather than "
                + "left to the binary that owns the grammar")
        XCTAssertFalse(PeerLease.clockHasPassed("99:99", now: noon, calendar: calendar))
    }

    /// The press opens the sheet; Save is the only writer.
    func testAddALeaseOpensTheSheetAndWritesNothing() throws {
        let pane = try source("apps/macos/Sources/TcrBar/PeersSettingsView.swift")
        let sheet = try slice(
            pane, from: "private func macSheet(", to: "/// One lease: two lines")
        XCTAssertTrue(
            sheet.contains("editingLease = .new(peer: row.id)"),
            "Add a lease… no longer opens a draft, so the press is back to granting a live "
                + "lease over every account at the 7-day default")
        XCTAssertFalse(
            sheet.contains("controller.run(\n                            PeerCommand.lend("),
            "Add a lease… runs a lend on the press again")
        XCTAssertTrue(
            sheet.contains("controller.run(edited.arguments)"),
            "Save no longer writes the draft it was handed")
        let row = try slice(pane, from: "private func leaseRow(", to: "/// The three ends")
        XCTAssertTrue(
            row.contains("editingLease = LeaseDraft(editing: grant, peer: peer)"),
            "the amount menu offers only the three shipped defaults again, so no fraction "
                + "outside them is reachable")
    }

    // MARK: - Item 5: Block reaches a row that is not knocking

    /// The verb takes the same argv either way, and each factory names what it
    /// is handed.
    func testBlockTakesAnAddressAsWellAsAnInstance() {
        XCTAssertEqual(
            PeerCommand.block(address: "10.0.1.24"), ["peer", "block", "10.0.1.24"])
        XCTAssertEqual(
            PeerCommand.block(instance: "8f2c1ad63b0e4471"),
            ["peer", "block", "8f2c1ad63b0e4471"],
            "the knock row's own spelling changed, and `tcr peer block` is the one verb both "
                + "call")
    }

    /// A found row can be blocked, and its confirm is aimed at the address.
    func testAFoundRowCanBeBlocked() throws {
        let tab = try source("apps/macos/Sources/TcrBar/PanelV4/PeersTabV4.swift")
        let menu = try slice(
            tab, from: "private func rowMenu(", to: "/// The glyph the live `Menu`")
        XCTAssertTrue(
            menu.contains("Button(\"Block \\(address)…\", role: .destructive) { blocking = row }"),
            "a found row cannot be blocked again, so an address can only be banned while it "
                + "is knocking")
        XCTAssertTrue(
            menu.contains("row.trust != .trusted, let address = row.address"),
            "the menu is offered without an address to aim at, or on a trusted row whose "
                + "trailing column has no width for it")
        XCTAssertTrue(
            menu.contains("if snapshotMode {"),
            "the render harness draws a live Menu again, which ImageRenderer rasterises as "
                + "the prohibited placeholder")
        let confirm = try slice(
            tab, from: "\"Block this Mac?\"", to: "private var blockingIsPresented")
        XCTAssertTrue(
            confirm.contains("controller.run(PeerCommand.block(address: address))"),
            "the tab's Block confirm no longer runs the address-shaped verb")
    }

    /// A trusted Mac can be blocked from its own sheet, which is where every
    /// per-Mac act lives.
    func testATrustedMacCanBeBlockedFromItsSheet() throws {
        let pane = try source("apps/macos/Sources/TcrBar/PeersSettingsView.swift")
        let sheet = try slice(
            pane, from: "private func macSheet(", to: "/// One lease: two lines")
        XCTAssertTrue(
            sheet.contains("Button(\"Block…\", role: .destructive) { blocking = row }"),
            "a trusted Mac's sheet lost Block, so a Mac already trusted cannot be banned at "
                + "any click depth")
        let confirm = try slice(
            pane, from: "\"Block this Mac?\"",
            to: "/// `confirmationDialog(presenting:)` needs "
                + "its own")
        XCTAssertTrue(
            confirm.contains("controller.run(PeerCommand.block(address: address))"),
            "the pane's Block confirm no longer runs the address-shaped verb")
    }

    /// The row carries the address the verb needs, on both kinds of row.
    func testEveryRowCarriesItsAddress() throws {
        let tab = try source("apps/macos/Sources/TcrBar/PanelV4/PeersTabV4.swift")
        let builder = try slice(
            tab, from: "private static func row(", to: "private static func pills(")
        XCTAssertEqual(
            builder.components(separatedBy: "address: entry.address").count - 1, 2,
            "a row is built without the wire's own address, so Block has nothing to aim at: "
                + "a trusted row's id is a peer id and its title is a name, and the verb "
                + "takes neither")
    }

    // MARK: - Item 6: the borrower is told when the lease ends

    /// `until` and `ended` decode when they are there and are absent when
    /// they are not, which is the state the `tcr` in this tree answers in.
    func testABorrowedLeasesEndDecodesAndTolerateTheKeysMissing() throws {
        let withKeys = """
            {"peers":[{"name":"studio-mac","trusted":true,"serves":true,
            "until":1786003600,"ended":false}]}
            """
        let row = try XCTUnwrap(
            JSONDecoder().decode(PeerListDocument.self, from: Data(withKeys.utf8)).peers.first)
        XCTAssertEqual(row.until, 1_786_003_600)
        XCTAssertEqual(row.ended, false)

        let without = """
            {"peers":[{"name":"attic-nuc","trusted":true,"serves":true}]}
            """
        let old = try XCTUnwrap(
            JSONDecoder().decode(PeerListDocument.self, from: Data(without.utf8)).peers.first)
        XCTAssertNil(old.until, "a tcr that does not send an end is read as having one")
        XCTAssertNil(
            old.ended,
            "an absent `ended` decodes to false, so a build that knows nothing about ends "
                + "asserts that no lease has one")
    }

    /// The row says when the work stops, and says nothing when there is
    /// nothing to say.
    func testTheBorrowerRowNamesTheHourTheWorkStops() {
        let now = Date(timeIntervalSince1970: 1_786_000_000)
        let inAnHour = PeerListDocument.PeerEntry(
            name: "studio-mac", trusted: true, serves: true, until: 1_786_003_600)
        XCTAssertEqual(inAnHour.endsInSentence(now: now), "This lease ends in 1h.")
        XCTAssertFalse(inAnHour.leaseHasEnded(now: now))

        let noEnd = PeerListDocument.PeerEntry(name: "attic-nuc", trusted: true, serves: true)
        XCTAssertNil(noEnd.endsInSentence(now: now), "a lease with no end is given one")

        let passed = PeerListDocument.PeerEntry(
            name: "loft-mini", trusted: true, serves: true, until: 1_785_998_200)
        XCTAssertNil(
            passed.endsInSentence(now: now),
            "an end that has passed is announced as still ahead")
        XCTAssertTrue(passed.leaseHasEnded(now: now))
    }

    /// The producer's own word wins over this panel's clock.
    func testTheProducersEndedFlagWinsOverTheClock() {
        let now = Date(timeIntervalSince1970: 1_786_000_000)
        let stillRunning = PeerListDocument.PeerEntry(
            name: "studio-mac", trusted: true, serves: true, until: 1_785_998_200, ended: false)
        XCTAssertFalse(
            stillRunning.leaseHasEnded(now: now),
            "this panel's arithmetic about somebody else's clock overrules the lender's own "
                + "answer")
        let saidEnded = PeerListDocument.PeerEntry(
            name: "studio-mac", trusted: true, serves: true, ended: true)
        XCTAssertTrue(saidEnded.leaseHasEnded(now: now))
    }

    /// The clause is appended in ONE place, so no meter sentence can be
    /// written without it.
    func testTheEndClauseIsAppendedOnce() throws {
        let tab = try source("apps/macos/Sources/TcrBar/PanelV4/PeersTabV4.swift")
        let meter = try slice(
            tab, from: "private static func meter(", to: "// MARK: - Reading it")
        XCTAssertEqual(
            meter.components(separatedBy: "entry.endsInSentence(now: now)").count - 1, 1,
            "the end sentence is computed more than once in the meter builder, which is four "
                + "chances to forget it")
        XCTAssertEqual(
            meter.components(separatedBy: "+ ends))").count - 1, 4,
            "one of the four lease sentences no longer carries the end clause, so the row it "
                + "draws never names the hour the work stops")
    }

    // MARK: - Item 7: an ended lease is visible on the tab

    /// The ended arm draws two sub lines and no bar, and says so to the height
    /// budget.
    func testTheEndedMeterHasItsOwnShape() {
        let ended = PeerMeter.ended(
            LeaseEnded(when: "17:30", sentence: "It ended.", relendArguments: nil))
        XCTAssertTrue(ended.isEnded)
        XCTAssertFalse(PeerMeter.none(nil).isEnded)
        XCTAssertFalse(
            ended.rowShape.hasMeter,
            "an ended lease draws a bar, and a bar reading zero says nothing is happening "
                + "right now rather than nothing will happen again")
        XCTAssertEqual(ended.rowShape.subLines, 2)
        XCTAssertEqual(
            LeaseEnded(when: "17:30", sentence: "It ended.").label, "ended 17:30")
        XCTAssertEqual(
            LeaseEnded(when: nil, sentence: "It ended.").label, "ended",
            "a lease that ended at an hour nobody reported gets a guessed clock")
    }

    /// Which direction ended, and who may re-lend it.
    func testAnEndedLeaseKnowsWhichDirectionItWas() {
        let now = Date(timeIntervalSince1970: 1_786_000_000)
        let calendar = Calendar(identifier: .gregorian)
        let running = PeerListDocument.PeerEntry(
            id: "tcr-4b8we1r0zp", name: "studio-mac", trusted: true, serves: true,
            until: 1_786_003_600)
        XCTAssertNil(
            LeaseEnded.forEntry(running, title: "studio-mac", now: now),
            "a lease still running is drawn as ended")

        let borrowed = PeerListDocument.PeerEntry(
            id: "tcr-4b8we1r0zp", name: "studio-mac", trusted: true, serves: true,
            until: 1_785_998_200, ended: true)
        let borrowedEnded = LeaseEnded.forEntry(
            borrowed, title: "studio-mac", now: now, calendar: calendar)
        XCTAssertNotNil(borrowedEnded, "a borrowed lease that ended is drawn as running")
        XCTAssertNil(
            borrowedEnded?.relendArguments,
            "Re-lend is offered on a lease this Mac BORROWED, which is the other Mac's act "
                + "to perform")

        let oneLive = PeerListDocument.PeerEntry(
            id: "tcr-92hbq5t7yv", name: "attic-nuc", trusted: true,
            lend: [
                PeerLendGrant(
                    leaseId: "ls-4b1f", scope: .all, window: .week, fraction: 0.2,
                    until: 1_785_998_200, ended: true),
                PeerLendGrant(
                    leaseId: "ls-9c02", scope: .all, window: .week, fraction: 0.2),
            ])
        XCTAssertNil(
            LeaseEnded.forEntry(oneLive, title: "attic-nuc", now: now),
            "a Mac with one live lease and one expired one is drawn as ended, and it is still "
                + "serving")

        let allEnded = PeerListDocument.PeerEntry(
            id: "tcr-92hbq5t7yv", name: "attic-nuc", trusted: true,
            lend: [
                PeerLendGrant(
                    leaseId: "ls-4b1f", scope: .all, window: .week, fraction: 0.2,
                    until: 1_785_998_200, ended: true)
            ])
        let lent = LeaseEnded.forEntry(
            allEnded, title: "attic-nuc", now: now, calendar: calendar)
        XCTAssertEqual(
            lent?.relendArguments,
            ["peer", "lend", "tcr-92hbq5t7yv", "--relend", "ls-4b1f"],
            "the lender's ended row lost the one control that puts the lease back")
        XCTAssertNotNil(lent?.when, "an ended lease with a clock reports none")
    }

    /// The tab has an ended arm at all and greys the row.
    func testTheTabDrawsAnEndedLease() throws {
        let tab = try source("apps/macos/Sources/TcrBar/PanelV4/PeersTabV4.swift")
        XCTAssertTrue(
            tab.contains("if let ended = LeaseEnded.forEntry(entry, title: title, now: now) {"),
            "the meter builder no longer asks whether the lease has ended, so a Mac whose "
                + "only lease expired keeps its meter and a sentence about the offer standing")
        let card = try slice(
            tab, from: "private func peerCard(", to: "/// A found row's own menu")
        XCTAssertTrue(
            card.contains(".opacity(row.meter.isEnded ? V4.endedRowOpacity : 1)"),
            "the ended row is no longer greyed, so it reads as live")
        XCTAssertTrue(
            tab.contains("pills(entry, awake: seen.awake, ended: meter.isEnded)"),
            "the pills derive ended for themselves again, so the row can draw a live pill "
                + "over a dead meter")
    }

    /// The pill says the lease is over instead of claiming a live grant.
    func testAnEndedRowDropsItsGrantPill() throws {
        let tab = try source("apps/macos/Sources/TcrBar/PanelV4/PeersTabV4.swift")
        let pills = try slice(
            tab, from: "private static func pills(", to: "/// Which meter a row gets")
        XCTAssertTrue(
            pills.contains("pills.append((\"lease ended\", .neutral))"),
            "an ended row keeps its SERVES pill, which tells the operator a grant is live "
                + "when it is over")
        let endedFirst = try XCTUnwrap(pills.range(of: "if ended {")).lowerBound
        let headroom = try XCTUnwrap(pills.range(of: "if entry.noHeadroom {")).lowerBound
        XCTAssertTrue(
            endedFirst < headroom,
            "another state takes the second pill ahead of the ended one")
    }

    /// Both surfaces grey one state by one amount.
    func testTheEndedGreyIsOneToken() throws {
        let pane = try source("apps/macos/Sources/TcrBar/PeersSettingsView.swift")
        XCTAssertTrue(
            pane.contains(".opacity(ended ? V4.endedRowOpacity : 1)"),
            "the sheet's ended lease row carries its own literal grey again, so the tab and "
                + "the sheet can drift")
    }

    // MARK: - Item 8: the fixtures

    /// The knock card, the waiting row and both ended rows have a scene.
    ///
    /// A card nobody renders is a card nobody reviews: the knock card carries
    /// three controls and the only sentence on the tab about what Accept does,
    /// and it had no tab fixture at all.
    func testEveryNewStateHasAPanelFixture() throws {
        let harness = try source("apps/macos/Sources/TcrBar/RenderStates.swift")
        let scenes = try slice(
            harness, from: "private static var peerScenes:", to: "/// The fixture Settings")
        for scene in [
            "52-peers-knock", "53-peers-waiting", "54-peers-lease-ended",
            "55-peers-lease-ends-soon",
        ] {
            XCTAssertTrue(
                scenes.contains("\"\(scene)\""),
                "\(scene) has no fixture, so the state it draws is unreviewable")
        }
        XCTAssertTrue(
            scenes.contains("PeersSnapshotBuilder.waiting("),
            "the waiting fixture is hand-built rather than passed through the overlay the "
                + "live controller applies, so it can picture a row the panel would not draw")
    }

    /// The two sheet scenes are rendered through the pane's own presentation
    /// path, and the capture takes the SHEET's window.
    func testTheSheetScenesAreRenderedAndCaptured() throws {
        let harness = try source("apps/macos/Sources/TcrBar/RenderSettings.swift")
        XCTAssertTrue(
            harness.contains("case defaults = \"peers-defaults-sheet\"")
                && harness.contains("case mac = \"peers-mac-sheet\""),
            "scenes 62 and 63 have no render scene")
        XCTAssertTrue(
            harness.contains("while let attached = target.attachedSheet { target = attached }"),
            "the capture takes the parent window again, which pictures the pane under a grey "
                + "scrim with no sheet in it")
        XCTAssertTrue(
            harness.contains("if sheet != nil && target === window {"),
            "a scene whose sheet never presented is captured anyway, which writes a PNG that "
                + "looks like a successful render of the wrong thing")
        let pane = try source("apps/macos/Sources/TcrBar/PeersSettingsView.swift")
        XCTAssertTrue(
            pane.contains("switch RenderSettings.requestedSheet {"),
            "the pane no longer opens the sheet a render run asked for")
        XCTAssertTrue(
            pane.contains(
                "RenderSettings.requestedDirectory() != nil ? RenderStates.peerNow : Date()"),
            "the pane judges ends against the real clock under a render run, so every fixture "
                + "lease reads as expired and scene 63 pictures three ended rows")
    }

    // MARK: - Helpers

    private func repoRoot() -> URL {
        URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent()  // this file -> TcrBarTests
            .deletingLastPathComponent()  // TcrBarTests -> Tests
            .deletingLastPathComponent()  // Tests -> apps/macos
            .deletingLastPathComponent()  // apps/macos -> apps
            .deletingLastPathComponent()  // apps -> repo root
    }

    private func source(_ relativePath: String) throws -> String {
        try String(
            contentsOf: repoRoot().appendingPathComponent(relativePath), encoding: .utf8)
    }

    private func slice(_ contents: String, from: String, to: String) throws -> String {
        guard let start = contents.range(of: from) else { throw Anchor.missing(from) }
        let rest = contents[start.upperBound...]
        guard let end = rest.range(of: to) else { throw Anchor.missing(to) }
        return String(rest[..<end.lowerBound])
    }

    private enum Anchor: Error, CustomStringConvertible {
        case missing(String)

        var description: String {
            switch self {
            case .missing(let anchor):
                return "anchor no longer in the source: \(anchor)"
            }
        }
    }
}
