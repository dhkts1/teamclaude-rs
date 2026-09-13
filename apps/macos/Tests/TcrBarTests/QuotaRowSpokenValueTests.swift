import XCTest

@testable import TcrBarCore

/// What a listener hears on a quota row, and which windows a card draws one for.
///
/// The row's per-window verdict used to ride on hue alone: the bar fill and the
/// reset caption both turn amber at the near threshold and red past it, and two
/// rows with identical wording differed only by `#ffd16b` against `#94928d`.
/// The row's `accessibilityLabel` said `"7d window, 94% used"` and, being an
/// explicit label on a `.combine`d element, OVERRODE the children — so the
/// `resets 3d 22h` caption, the row's one actionable fact, was spoken by
/// nobody.
///
/// `swift test` links `TcrBarCore` only, so the row itself cannot be built
/// here; what it speaks is `QuotaFormat.spokenWindowValue`, which can.
///
/// Account names are obviously fake; this repository is public.
final class QuotaRowSpokenValueTests: XCTestCase {

    /// A fixed clock, so `resets …` is deterministic.
    private let now = Date(timeIntervalSince1970: 1_757_000_000)

    private func resetIn(minutes: Int) -> Int64 {
        Int64((now.timeIntervalSince1970 + Double(minutes) * 60) * 1000)
    }

    // MARK: - The spoken value (review #3)

    func testTheStateWordIsSpokenNotOnlyTinted() {
        XCTAssertEqual(
            QuotaFormat.spokenWindowValue(
                value: 0.94, state: .near, resetAtMs: resetIn(minutes: 47), now: now),
            "94% used, near the limit, resets 47m",
            "the near verdict reached a listener through nothing but the bar's hue")
    }

    func testTheResetCountdownIsSpoken() {
        let spoken = QuotaFormat.spokenWindowValue(
            value: 0.12, state: .ok, resetAtMs: resetIn(minutes: 130), now: now)
        XCTAssertTrue(
            spoken.contains("resets"),
            "the countdown is the row's one actionable fact; the old explicit "
                + "label overrode the combined children that carried it")
        XCTAssertEqual(spoken, "12% used, within limit, resets 2h 10m")
    }

    func testASpentWindowSaysSoInWords() {
        XCTAssertEqual(
            QuotaFormat.spokenWindowValue(
                value: 1.0, state: .spent, resetAtMs: resetIn(minutes: 42), now: now),
            "100% used, spent, resets 42m")
    }

    /// A window with no live reset says the figure and the state, and does not
    /// invent a countdown.
    func testAWindowWithNoLiveResetSpeaksNoCountdown() {
        XCTAssertEqual(
            QuotaFormat.spokenWindowValue(value: 0.22, state: .ok, resetAtMs: nil, now: now),
            "22% used, within limit")
    }

    /// Nothing measured is a different sentence, not a missing clause. `"0%
    /// used"` would report a reading nobody took.
    func testAnUnmeasuredWindowSaysNeverMeasured() {
        XCTAssertEqual(
            QuotaFormat.spokenWindowValue(value: nil, state: nil, resetAtMs: nil, now: now),
            "never measured")
    }

    /// A state word this build cannot name is spoken verbatim rather than
    /// translated into one of the three it knows.
    func testAnUnknownStateTokenIsSpokenVerbatim() {
        XCTAssertEqual(
            QuotaFormat.spokenWindowValue(
                value: 0.5, state: .unknown("throttling"), resetAtMs: nil, now: now),
            "50% used, throttling")
    }

    /// The prose register, not the pill's. `"near"` alone in a sentence parses
    /// as an adjective with nothing to modify.
    func testTheSpokenWordsAreTheProseRegisterNotTheWireTokens() {
        XCTAssertEqual(QuotaState.ok.spokenWord, "within limit")
        XCTAssertEqual(QuotaState.near.spokenWord, "near the limit")
        XCTAssertEqual(QuotaState.spent.spokenWord, "spent")
        XCTAssertEqual(QuotaState.ok.token, "ok")
    }

    // MARK: - The Fable weekly window (Gil: "why i dont see fable like we had before?")

    /// The Fable window's tint may never borrow the composite `quotaState` the
    /// way `quotaBarTintSource(for:)` does for an old server: it gates Fable
    /// requests alone, so the composite is not a weaker reading of it — it is
    /// a reading of something else.
    func testTheFableWindowNeverBorrowsTheCompositeState() {
        let spentElsewhere = fableAccount(
            "alice@example.com", quotaState: .spent, sevenDayOi: 0.71, sevenDayOiState: nil)
        XCTAssertEqual(
            spentElsewhere.fableBarTintSource, .measuredWithoutState,
            "the account is spent on its composite window; the Fable bar may "
                + "not be painted red on that account's behalf")
    }

    func testTheFableWindowWearsItsOwnStateWhenTheServerSendsOne() {
        let account = fableAccount(
            "bob@example.com", quotaState: .ok, sevenDayOi: 0.71, sevenDayOiState: .near)
        XCTAssertEqual(account.fableBarTintSource, .state(.near))
    }

    /// No figure, no window. An empty `fable` track on an account that has no
    /// such window would claim a window that does not exist — the opposite of
    /// the `5h`/`7d` rule, where an empty track IS the fact.
    func testAnAccountWithNoFableWindowHasNothingToDraw() {
        let account = fableAccount(
            "carol@example.com", quotaState: .ok, sevenDayOi: nil, sevenDayOiState: nil)
        XCTAssertNil(account.sevenDayOi, "the card draws the row only when this is non-nil")
        XCTAssertEqual(account.fableBarTintSource, .unmeasured)
        XCTAssertNil(account.fableWeeklyLabel(now: now))
    }

    /// The two parity-fixture accounts, as the scene renders them: `henry10`
    /// carries the window, `henry5` does not.
    func testTheFableRowIsSpokenTheSameWayEveryOtherWindowIs() {
        let account = fableAccount(
            "dave@example.com", quotaState: .ok, sevenDayOi: 0.71, sevenDayOiState: .near,
            sevenDayOiResetAtMs: resetIn(minutes: 6_498))
        XCTAssertEqual(
            QuotaFormat.spokenWindowValue(
                value: account.sevenDayOi, state: account.sevenDayOiState,
                resetAtMs: account.sevenDayOiResetAtMs, now: now),
            "71% used, near the limit, resets 4d 12h")
    }
}

private func fableAccount(
    _ name: String,
    quotaState: QuotaState,
    sevenDayOi: Double?,
    sevenDayOiState: QuotaState?,
    sevenDayOiResetAtMs: Int64? = nil
) -> Account {
    Account(
        name: name,
        priority: 1,
        status: "active",
        disabled: false,
        quota: 0.5,
        quotaState: quotaState,
        fiveHour: 0.5,
        sevenDay: 0.5,
        sevenDayOi: sevenDayOi,
        sevenDayOiState: sevenDayOiState,
        sevenDayOiResetAtMs: sevenDayOiResetAtMs,
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
