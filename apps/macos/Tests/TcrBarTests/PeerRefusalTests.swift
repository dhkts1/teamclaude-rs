import XCTest

@testable import TcrBarCore

/// What a refused verb says, and in which order.
///
/// The banner led with a command line and an exit code and reached the half a
/// person can act on about eighty characters in: `tcr peer share on failed
/// (exit 1): peer share: refused, no Mac is trusted yet, so there is nobody to
/// share with`. The machine's half is what a bug report needs and a person
/// does not, so it moves behind Details and the sentence leads.
final class PeerRefusalTests: XCTestCase {

    private let raw =
        "tcr peer share on failed (exit 1): peer share: refused, no Mac is trusted yet, so "
        + "there is nobody to share with"

    /// The headline names the VERB that was refused, so a banner cannot say
    /// "that" about a press three seconds old.
    func testTheHeadlineNamesTheVerbThatWasRefused() {
        var refusal = PeerRefusal()
        refusal.refused(raw, verb: ["peer", "share", "on"])
        XCTAssertEqual(refusal.headline, "Sharing was refused")

        refusal.refused(
            "tcr peer block 192.0.2.7 failed (exit 1): peer block: refused, no",
            verb: ["peer", "block", "192.0.2.7"])
        XCTAssertEqual(refusal.headline, "Blocking was refused")
    }

    /// A verb this build has no word for, and a refusal that arrived with no
    /// argv at all, both keep the sentence the banner has always had rather
    /// than guessing a noun.
    func testAnUnknownVerbKeepsTheOldHeadline() {
        var refusal = PeerRefusal()
        refusal.refused(raw)
        XCTAssertEqual(refusal.headline, "That was refused")
        refusal.refused(raw, verb: ["peer", "vacuum"])
        XCTAssertEqual(refusal.headline, "That was refused")
    }

    /// The body is the half a person can act on, in the CLI's own words: the
    /// command line, the exit code and the repeated verb come off the front,
    /// and nothing is invented to replace them.
    func testTheBodyIsThePersonReadableHalf() {
        var refusal = PeerRefusal()
        refusal.refused(raw, verb: ["peer", "share", "on"])
        XCTAssertEqual(
            refusal.body, "No Mac is trusted yet, so there is nobody to share with.")
        XCTAssertEqual(
            refusal.message, raw,
            "the raw line is paraphrased away, and it is the one thing a bug report needs")
    }

    /// A message in some other shape is shown whole rather than cut at a
    /// pattern it does not have: a banner that trims the wrong half tells a
    /// person nothing at all.
    func testAMessageInAnotherShapeIsKeptWhole() {
        var refusal = PeerRefusal()
        // Not re-cased either: `tcr` sentence-cased reads `Tcr`, which is not
        // the name of the tool.
        refusal.refused("tcr not found (searched 4 locations)")
        XCTAssertEqual(refusal.body, "tcr not found (searched 4 locations)")

        refusal.refused("")
        XCTAssertNil(refusal.body, "an empty refusal draws a full stop on its own")
    }

    /// Both ways a refusal goes still clear everything it set.
    func testAnsweringARefusalClearsItsVerbToo() {
        var refusal = PeerRefusal()
        refusal.refused(raw, verb: ["peer", "share", "on"])
        refusal.succeeded()
        XCTAssertNil(refusal.message)
        XCTAssertNil(refusal.body)
        XCTAssertEqual(
            refusal.headline, "That was refused",
            "a dismissed refusal keeps the last verb, so the next one can open with somebody "
                + "else's noun")
        XCTAssertFalse(refusal.isShowing)
    }
}
