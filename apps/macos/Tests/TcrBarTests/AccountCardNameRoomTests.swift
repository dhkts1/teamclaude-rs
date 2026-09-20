import AppKit
import XCTest

@testable import TcrBarCore

/// How much of the card's header line the account name actually gets.
///
/// The name is the one thing that tells two cards apart, and it was the one
/// thing being truncated: an ordinary address beside its plan ran out of room
/// on a perfectly healthy card and drew as `alice @ex…`. The header is a fixed
/// 372 pt wide, every pill in it is `fixedSize()` and so cannot give ground,
/// and the pills were drawn before anyone asked whether they said anything.
/// Dropping the pool word off the healthy card is what hands that width back.
///
/// Measured rather than rendered, the way ``SegmentedTabsFitTests`` measures a
/// strip it cannot instantiate: this bundle links `TcrBarCore` only, so the
/// geometry comes from `V4`'s own tokens read out of the app target's source,
/// at the real system font and real string widths.
///
/// Both arms are asserted on purpose. "It fits now" alone would pass just as
/// happily against a card that always had room, so the second arm re-measures
/// the same name against the header as it was, with the pool pill still in it,
/// and fails if THAT fits too.
///
/// Account names are obviously fake; this repository is public.
final class AccountCardNameRoomTests: XCTestCase {

    /// A plain address and a plan, nothing unusual: the case the owner saw
    /// truncated on the released panel.
    private let accountName = "alice@example.com"
    private let planLabel = "Max 20x"

    func testAnOrdinaryNameAndItsPlanFitTheHeaderOnceTheRotatingPillIsGone() throws {
        let wanted = try nameRowIdealWidth(name: accountName, plan: planLabel)
        let roomNow = try nameRoom(pills: ["OK"])
        let roomBefore = try nameRoom(pills: ["Rotating", "OK"])

        XCTAssertLessThanOrEqual(
            wanted, roomNow,
            "\"\(accountName)\" plus \"\(planLabel)\" wants \(pt(wanted)) and the header leaves "
                + "\(pt(roomNow)): the name truncates on an ordinary healthy card")
        XCTAssertGreaterThan(
            wanted, roomBefore,
            "the header still had room for this name WITH a ROTATING pill "
                + "(\(pt(roomBefore))), so this test is measuring nothing: pick a name that "
                + "the old header actually truncated")
    }

    /// The geometry above only holds because the model stopped handing the card
    /// a word for the ordinary case. Pinned here too, so a change that puts the
    /// pill back cannot leave this file passing.
    func testAHealthyAccountHandsTheCardNoPoolWordToDraw() {
        XCTAssertNil(RotationState.rotating.label)
        XCTAssertEqual(RotationState.groupOnly.label, "Group only")
    }

    /// The reserved card keeps its pill, so it keeps paying for it: the name
    /// there has the pill's width less to work with, and that is the trade the
    /// rarer, unguessable state is worth.
    func testTheReservedCardStillSpendsItsWidthOnTheWordItKeeps() throws {
        let reserved = try nameRoom(pills: ["Group only", "OK"])
        let healthy = try nameRoom(pills: ["OK"])
        XCTAssertLessThan(reserved, healthy)
    }

    /// The control account's card is the crowded one: it carries a second pill
    /// that no ordinary card does, and it is the account every quota figure on
    /// the panel is measured through, so it is the worst card to be unable to
    /// name. Its whole name fits; its plan is what gives way.
    func testTheControlCardKeepsItsWholeNameAndGivesUpThePlanInstead() throws {
        let room = try nameRoom(pills: ["Control", "OK"])
        let nameOnly = try nameRowIdealWidth(name: accountName, plan: nil)
        let withPlan = try nameRowIdealWidth(name: accountName, plan: planLabel)

        XCTAssertLessThanOrEqual(
            nameOnly, room,
            "the control card cannot draw \"\(accountName)\" whole: it wants "
                + "\(pt(nameOnly)) and the header leaves \(pt(room))")
        XCTAssertGreaterThan(
            withPlan, room,
            "name and plan both fit here, so nothing has to give way and this test is "
                + "measuring nothing")
    }

    /// Which piece gives way is a priority order, not an arithmetic fact, so
    /// the arithmetic above cannot see it. Pinned at the source: strictly
    /// descending down the row, name first, then the plan, then the handle.
    ///
    /// The plan sits ABOVE the handle because it is drawn only when it has
    /// been measured to fit beside a handle at its floor. Below it, the plan is
    /// squeezed first and a card with room for both draws a one-letter plan
    /// over a barely-shortened handle, which is what shipped for one commit.
    func testTheNameOutranksThePlanAndThePlanOutranksTheHandle() throws {
        let source = try panelSource("PanelV4/AccountCard.swift")
        let squashed = source.components(separatedBy: .whitespacesAndNewlines).joined()
        let name = try priority(after: "Text(localPart)", in: squashed)
        let domain = try priority(after: "Text(domain)", in: squashed)
        let plan = try priority(after: "MuteText(text:plan)", in: squashed)
        XCTAssertGreaterThan(
            name, plan,
            "the plan no longer gives way before the name (name \(name), plan \(plan))")
        XCTAssertGreaterThan(
            plan, domain,
            "the handle no longer gives way before the plan, so a plan this row measured room "
                + "for can still be cut to a letter (plan \(plan), handle \(domain))")
    }

    // MARK: - Whole or not at all

    /// The card that drew `T`. Measured, it was never short of room: a grouped
    /// card with a short name has 148.3 pt and needs 113.2 beside a handle at
    /// its floor. What cut the plan was the layout order, not the width, so the
    /// plan is drawn here and the handle is what gives way.
    func testTheGroupedCardThatDrewOneLetterHasTheRoomForItsWholePlan() throws {
        let available = try nameRoom(pills: ["Group only", "OK"], compact: true)
        XCTAssertTrue(
            try drawsPlan(name: "bob@example.com", plan: "Team 5x", available: available),
            "a grouped card with \(pt(available)) draws no plan, though a whole one fits "
                + "beside a truncated handle")
    }

    /// The other arm, and a real card rather than a contrived one: the same
    /// grouped width with a longer name and a longer plan genuinely does not
    /// fit, so nothing is drawn rather than a piece of something.
    func testAGroupedCardWithNoRoomDrawsNoPlanAtAll() throws {
        let available = try nameRoom(pills: ["Group only", "OK"], compact: true)
        XCTAssertFalse(
            try drawsPlan(
                name: "member2@example.com", plan: "Team Standard", available: available),
            "the card still draws a plan it would have to cut: \(pt(available)) available")
    }

    /// The loose card in the same fixture has the room, so it keeps its plan
    /// whole. Both halves of the rule in two tests, because a rule that only
    /// ever hides things is indistinguishable from deleting the plan.
    func testALooseCardWithRoomDrawsItsPlanWhole() throws {
        let available = try nameRoom(pills: ["OK"])
        XCTAssertTrue(
            try drawsPlan(name: "alice@example.com", plan: "Max 20x", available: available),
            "the ordinary healthy card no longer draws its plan: \(pt(available)) available")
    }

    /// A short name with nothing beside it is the easiest case there is, and a
    /// rule that got it wrong would be hiding the plan on every card.
    func testAShortNameWithNoPillsDrawsItsPlan() throws {
        XCTAssertTrue(
            try drawsPlan(
                name: "gil@example.com", plan: "Team 5x", available: try nameRoom(pills: [])))
    }

    /// The handle is the piece that still truncates, so the plan is judged
    /// against a handle shrunk to its floor rather than its full width. Without
    /// that, a long domain would take the plan down with it even on a card with
    /// room for both.
    func testThePlanIsJudgedAgainstATruncatedHandleNotAFullOne() throws {
        let available = try nameRoom(pills: ["OK"])
        let nameSize = try densityToken("nameSize")
        XCTAssertLessThan(
            NameRowFit.handleFloorWidth(size: nameSize),
            measure("@a-very-long-domain-name.example.com", size: nameSize, weight: .medium),
            "the handle floor is not below a full long domain, so it is not a floor")
        XCTAssertTrue(
            try drawsPlan(
                name: "gil@a-very-long-domain-name.example.com", plan: "Team 5x",
                available: available),
            "a long handle took the plan down with it, though the handle is the piece that "
                + "gives way first")
    }

    private func drawsPlan(name: String, plan: String, available: CGFloat) throws -> Bool {
        let at = name.firstIndex(of: "@")
        return NameRowFit.drawsPlan(
            localPart: at.map { String(name[name.startIndex..<$0]) } ?? name,
            domain: at.map { String(name[$0...]) }, plan: plan, available: available,
            nameSize: try densityToken("nameSize"), planSize: try densityToken("muteSize"),
            gap: try token("tabGap"))
    }

    /// The first `.layoutPriority(<n>)` after a marker, in whitespace-squashed
    /// source.
    private func priority(after marker: String, in squashed: String) throws -> Double {
        let tail = try XCTUnwrap(
            squashed.range(of: marker).map { String(squashed[$0.upperBound...]) },
            "`\(marker)` is no longer in AccountCard's name row in the shape this test reads")
        let value = try XCTUnwrap(
            firstCapture(of: "^[\\s\\S]{0,400}?\\.layoutPriority\\((-?[0-9.]+)\\)", in: tail),
            "`\(marker)` carries no `.layoutPriority(...)` within the row, so the order the "
                + "card documents is not the order it lays out")
        return try XCTUnwrap(Double(value))
    }

    // MARK: - The header's own arithmetic

    /// What is left of the card's inner width for the name row, once the
    /// trailing pills and the actions gear have taken theirs.
    ///
    /// `AccountCard`'s header is `HStack(spacing: pillGap) { V4Row { nameRow }
    /// trailing: { pills } ; actions() }`, `V4Row` itself an `HStack(spacing:
    /// rowGap)` whose trailing column is `fixedSize()`, so every term below is
    /// a width the name row can never take back.
    private func nameRoom(pills: [String], compact: Bool = false) throws -> CGFloat {
        let panelWidth = try token("panelWidth")
        let panelPaddingSide = try token("panelPaddingSide")
        let cardInsetH = try densityToken("cardPaddingH") + (try token("panelBorderWidth"))
        let rowGap = try densityToken("rowGap")
        let pillGap = try token("pillGap")
        // A card inside a group box loses that box's side padding and stroke
        // on both edges before it starts.
        let groupInset =
            compact ? 2 * ((try token("groupPaddingSide")) + (try token("groupStroke"))) : 0
        let inner = panelWidth - 2 * panelPaddingSide - groupInset - 2 * cardInsetH
        let block =
            try pills.map { try pillWidth($0) }.reduce(0, +)
            + pillGap * CGFloat(max(pills.count - 1, 0))
        return inner - pillGap - NameRowFit.actionsSlotWidth - rowGap - block
    }

    /// An outlined `V4Pill`: the word uppercased at the pill font, its tracking
    /// once per character, and the horizontal padding on both sides. The border
    /// is `strokeBorder`, drawn inside, and adds nothing.
    private func pillWidth(_ text: String) throws -> CGFloat {
        let word = text.uppercased()
        return measure(word, size: try token("pillFontSize"), weight: .bold)
            + (try productToken("pillTracking")) * CGFloat(word.count)
            + 2 * (try token("pillPaddingH"))
    }

    /// The three pieces of ``AccountCard``'s name row at their ideal widths:
    /// the local part, `@domain`, and the plan, `tabGap` apart.
    private func nameRowIdealWidth(name: String, plan: String?) throws -> CGFloat {
        let nameSize = try densityToken("nameSize")
        let tracking = try productToken("nameTracking")
        let tabGap = try token("tabGap")
        let at = name.firstIndex(of: "@")
        let local = at.map { String(name[name.startIndex..<$0]) } ?? name
        var width =
            measure(local, size: nameSize, weight: .semibold)
            + tracking * CGFloat(local.count)
        if let at {
            let domain = String(name[at...])
            width +=
                tabGap + measure(domain, size: nameSize, weight: .medium)
                + tracking * CGFloat(domain.count)
        }
        if let plan {
            width += tabGap + measure(plan, size: try densityToken("muteSize"), weight: .regular)
        }
        return width
    }

    /// The card's own measurement, not a second copy of it: the card decides
    /// what to draw with ``NameRowFit``, and a test that measured strings its
    /// own way would agree with the card only until one of them was edited.
    private func measure(_ string: String, size: CGFloat, weight: NSFont.Weight) -> CGFloat {
        NameRowFit.textWidth(string, size: size, weight: weight)
    }

    private func pt(_ value: CGFloat) -> String {
        String(format: "%.1f pt", value)
    }

    // MARK: - Reading the tokens out of the app target's source

    /// `static let <name>: CGFloat = <number>`.
    private func token(_ name: String) throws -> CGFloat {
        let value = try XCTUnwrap(
            firstCapture(of: "static let \(name): CGFloat = ([0-9.]+)\\b", in: try v4Source()),
            "V4.\(name) is no longer a plain `static let … = <number>`; teach this test the "
                + "new shape rather than measuring against a guess")
        return CGFloat(try XCTUnwrap(Double(value)))
    }

    /// `static let <name>: CGFloat = <a> * <b>`: the tracking tokens, which
    /// are written as the fraction and the size they came from.
    private func productToken(_ name: String) throws -> CGFloat {
        let squashed = try squashedV4Source()
        let pattern = "staticlet\(name):CGFloat=(-?[0-9.]+)\\*(-?[0-9.]+)"
        let source = squashed
        guard let regex = try? NSRegularExpression(pattern: pattern),
            let match = regex.firstMatch(
                in: source, range: NSRange(source.startIndex..., in: source)),
            let left = Range(match.range(at: 1), in: source),
            let right = Range(match.range(at: 2), in: source)
        else {
            XCTFail("V4.\(name) is no longer `<a> * <b>`")
            return 0
        }
        return CGFloat(try XCTUnwrap(Double(source[left])) * (try XCTUnwrap(Double(source[right]))))
    }

    /// `static var <name>: CGFloat { compact ? <a> : <b> }`: the comfortable
    /// branch, the larger of the two, so a fit proven here holds at Compact's
    /// tighter size as well.
    private func densityToken(_ name: String) throws -> CGFloat {
        let value = try XCTUnwrap(
            firstCapture(
                of: "staticvar\(name):CGFloat\\{compact\\?[0-9.]+:([0-9.]+)\\}",
                in: try squashedV4Source()),
            "V4.\(name) is no longer `compact ? <n> : <n>`")
        return CGFloat(try XCTUnwrap(Double(value)))
    }

    private func firstCapture(of pattern: String, in source: String) -> String? {
        guard let regex = try? NSRegularExpression(pattern: pattern),
            let match = regex.firstMatch(
                in: source, range: NSRange(source.startIndex..., in: source)),
            let range = Range(match.range(at: 1), in: source)
        else { return nil }
        return String(source[range])
    }

    private func squashedV4Source() throws -> String {
        try v4Source().components(separatedBy: .whitespacesAndNewlines).joined()
    }

    private func v4Source() throws -> String {
        try panelSource("PanelV4/V4.swift")
    }

    private func panelSource(_ relative: String) throws -> String {
        let repoRoot = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent()  // -> TcrBarTests
            .deletingLastPathComponent()  // -> Tests
            .deletingLastPathComponent()  // -> apps/macos
            .deletingLastPathComponent()  // -> apps
            .deletingLastPathComponent()  // -> repo root
        return try String(
            contentsOf: repoRoot.appendingPathComponent("apps/macos/Sources/TcrBar/\(relative)"),
            encoding: .utf8)
    }
}
