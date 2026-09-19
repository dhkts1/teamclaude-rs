import XCTest

@testable import TcrBarCore

/// The channel the menu bar reads a knock through, with the panel closed.
///
/// Every case here drives ``KnockReader/decode(_:)`` with bytes rather than a
/// process: the decision this type exists to make is what a document MEANS,
/// and the spawn around it is `TcrTool`'s, tested there.
final class KnockReaderTests: XCTestCase {

    private func output(_ json: String, exitCode: Int32 = 0) -> TcrTool.Output {
        TcrTool.Output(exitCode: exitCode, stdout: Data(json.utf8), stderr: "")
    }

    /// The document `tcr peer pending --json` prints, decoded down to the rows
    /// the bar counts. Addresses are documentation range: this repository is
    /// public.
    func testThePendingDocumentDecodesItsRows() {
        let read = KnockReader.decode(
            output(
                """
                {"pending":[
                  {"addr":"192.0.2.24:7766","instanceId":"8f2c1ad63b0e4471",
                   "proposedName":"loft-mini","wireVersion":1,
                   "firstSeenMs":1700000000000,"lastSeenMs":1700000030000},
                  {"addr":"192.0.2.31","instanceId":"3b1d90c47ae25f68",
                   "wireVersion":1,"firstSeenMs":1700000010000,"lastSeenMs":1700000040000}
                ],"muted":[],"banned":[]}
                """))
        guard case .read(let knocks) = read else {
            return XCTFail("a well-formed document read as a failure: \(read)")
        }
        XCTAssertEqual(knocks.count, 2)
        XCTAssertEqual(knocks[0].addr, "192.0.2.24:7766")
        XCTAssertEqual(knocks[0].proposedName, "loft-mini")
        XCTAssertNil(
            knocks[1].proposedName,
            "a knock that proposed no name must not decode to an empty string, which the "
                + "card would draw as a name")
    }

    /// An empty queue is a READ, and the honest one: nobody is asking.
    func testAnEmptyQueueIsARead() {
        XCTAssertEqual(
            KnockReader.decode(output(#"{"pending":[],"muted":[],"banned":[]}"#)), .read([]))
    }

    /// **The four absences that must never read as "nobody is asking".**
    ///
    /// A failed read leaves the last good rows standing; an empty list draws
    /// nothing. Conflating them is how a mark disappears from the bar while a
    /// stranger's Mac is still waiting on an answer.
    func testEveryFailureIsAFailureAndNeverAnEmptyQueue() {
        XCTAssertEqual(
            KnockReader.decode(output(#"{"pending":[]}"#, exitCode: 1)), .failed,
            "a non-zero exit read as an empty queue")
        XCTAssertEqual(
            KnockReader.decode(output("not json at all")), .failed,
            "output this build cannot parse read as an empty queue")
        XCTAssertEqual(
            KnockReader.decode(output("[]")), .failed,
            "a bare array is not this verb's document")
        XCTAssertEqual(
            KnockReader.decode(output(#"{"muted":[],"banned":[]}"#)), .failed,
            "a document with no pending key at all — an older tcr printing something else "
                + "entirely — read as nobody asking")
    }

    /// One unreadable row costs that row and never the Mac asking beside it,
    /// the rule ``Fleet/decode(_:)`` already follows for accounts.
    func testOneUnreadableRowCostsOnlyItself() {
        let read = KnockReader.decode(
            output(
                """
                {"pending":[
                  42,
                  {"addr":"192.0.2.24","instanceId":"8f2c1ad63b0e4471","wireVersion":1,
                   "firstSeenMs":1700000000000,"lastSeenMs":1700000030000}
                ]}
                """))
        guard case .read(let knocks) = read else {
            return XCTFail("one bad row cost the whole read: \(read)")
        }
        XCTAssertEqual(knocks.map(\.addr), ["192.0.2.24"])
    }

    /// The pinned reader spawns nothing and reports it has already read, which
    /// is what the render harness and the notifier's tests need.
    @MainActor
    func testThePinnedReaderPublishesItsKnocksWithoutSpawningAnything() {
        let reader = KnockReader(
            pinnedKnocks: [PeerKnock(addr: "192.0.2.24", instanceId: "8f2c1ad63b0e4471")])
        XCTAssertEqual(reader.knocks.count, 1)
        XCTAssertTrue(reader.hasRead)
        XCTAssertFalse(reader.lastReadFailed)
    }

    /// The verb is `tcr peer pending --json`, and it is the one that reads the
    /// peer STATE FILE. `peer status --json` asks the running proxy, so it
    /// answers "no knocks" whenever the proxy is down, which is the one
    /// silence this channel must not have.
    func testTheReaderRunsTheFileBackedVerb() {
        XCTAssertEqual(PeerCommand.pending, ["peer", "pending", "--json"])
    }
}
