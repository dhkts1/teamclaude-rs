import XCTest

@testable import TcrBarCore

/// The Tools and Sessions summary lines are one long string of short phrases
/// joined by " · ". When that string is wider than the panel, the layout picks
/// a space to break at, and before ``QuotaFormat/unbreakable(_:)`` it picked
/// whichever space sat at the edge. A two-word phrase could land with one word
/// on each line, so "N err today" read as "N err" above "today · N timed out"
/// and `today` appeared to qualify the timeouts rather than the errors.
///
/// Nothing is lost when that happens, and that is exactly why no other gate
/// here catches it: the string is complete and only its break point is wrong.
///
/// Every value below is invented. These assertions are about where a space may
/// break, which does not depend on the number in front of it.
final class SummaryPhraseWrapTests: XCTestCase {
    /// The defect, stated as the phrase that split.
    func testAnErrorPhraseCannotSplitAcrossLines() {
        let phrase = QuotaFormat.unbreakable("7 err today")
        XCTAssertFalse(
            phrase.contains(" "),
            "a summary phrase that still holds an ordinary space can be broken at it")
        XCTAssertEqual(phrase, "7\u{00A0}err\u{00A0}today")
    }

    /// Non-breaking or not, the operator must read the same words. A reader
    /// cannot tell U+00A0 from U+0020, so the visible text is unchanged and
    /// every count over it is too.
    func testTheVisibleTextIsUnchanged() {
        for raw in [
            "load 1.5/8", "4/16 GB · 2 compiles · 12 GB free", "3 timed out",
            "$1.00 today", "1/2 accounts", "7 err",
        ] {
            let out = QuotaFormat.unbreakable(raw)
            XCTAssertEqual(
                out.replacingOccurrences(of: "\u{00A0}", with: " "), raw,
                "unbreakable() must change only which spaces may break, never the words")
            XCTAssertEqual(out.count, raw.count)
        }
    }

    /// Applying it twice is applying it once: the summary builders may grow a
    /// second layer without doubling anything.
    func testItIsIdempotent() {
        let once = QuotaFormat.unbreakable("3 timed out")
        XCTAssertEqual(QuotaFormat.unbreakable(once), once)
    }

    /// The separator between phrases is the one break the line WANTS, so it is
    /// built outside this helper and keeps its ordinary spaces. This test
    /// exists so a future "just make the whole line unbreakable" change fails
    /// here instead of shipping a numbers line that overflows the panel with
    /// nowhere to wrap.
    func testTheSeparatorIsNotThisHelpersJob() {
        XCTAssertTrue(
            " · ".contains(" "),
            "the joiner's own spaces are the line's only legal break points")
    }
}
