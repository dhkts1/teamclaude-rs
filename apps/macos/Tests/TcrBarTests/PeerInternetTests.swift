import XCTest

@testable import TcrBarCore

/// The internet-reach switch's four states, the argv behind it, and Gil's
/// expiry rule: the line expires with the mapping, never a stale address.
final class PeerInternetTests: XCTestCase {
    private let now = Date(timeIntervalSince1970: 1_786_000_000)

    // MARK: Argv

    func testSwitchArgvIsTheStateItMovesTo() {
        XCTAssertEqual(PeerCommand.internet(on: true), ["peer", "internet", "on"])
        XCTAssertEqual(PeerCommand.internet(on: false), ["peer", "internet", "off"])
    }

    /// The probe asks for a mapping, in JSON. `--map` is what makes the answer
    /// a mapping at all; without it the verb reports `not asked for`, and the
    /// state line would never leave "Router did not answer".
    func testReachArgvAsksForAMappingAndJson() {
        XCTAssertEqual(PeerCommand.reach, ["peer", "reach", "--map", "--json"])
    }

    // MARK: Decoding what the verb printed

    func testDecodesAGrantedMapping() throws {
        let json = """
            {"ipv6":[],"gateway":"10.0.0.1","externalAddress":"203.0.113.44",
             "mapping":"tcp 51413 -> 7749 for 120s","listenPort":7749,
             "slot":42,"slotSeconds":600,"peers":[]}
            """
        let reading = try PeerReachReading.decode(Data(json.utf8), readAt: now)
        XCTAssertEqual(reading.externalAddress, "203.0.113.44")
        XCTAssertEqual(reading.listenPort, 7749)
        XCTAssertEqual(reading.mapping, .granted(externalPort: 51413, lifetimeSeconds: 120))
        XCTAssertNil(reading.heldMapping)
    }

    /// `heldMapping.expiresAtMs`, the absolute instant the SERVING process's
    /// keeper recorded (see `MappingRecord` in `src/peer/state.rs`).
    func testDecodesAHeldMapping() throws {
        let json = """
            {"externalAddress":"unavailable: no gateway","mapping":"not asked for (pass --map)",
             "listenPort":7749,
             "heldMapping":{"externalAddress":"203.0.113.44","externalPort":51413,
                             "internalPort":7749,"expiresAtMs":1786000120000}}
            """
        let reading = try PeerReachReading.decode(Data(json.utf8), readAt: now)
        let held = try XCTUnwrap(reading.heldMapping)
        XCTAssertEqual(held.externalAddress, "203.0.113.44")
        XCTAssertEqual(held.externalPort, 51413)
        XCTAssertEqual(held.internalPort, 7749)
        XCTAssertEqual(held.expiresAtMs, 1_786_000_120_000)
        XCTAssertEqual(held.expires, now.addingTimeInterval(120))
    }

    /// No serving process holds a mapping: `heldMapping` is `null` on the
    /// wire, and that decodes as `nil`, never a default that reads like a
    /// measurement.
    func testHeldMappingDecodesAsAbsentWhenNull() throws {
        let json = """
            {"externalAddress":"203.0.113.44","mapping":"not asked for (pass --map)",
             "listenPort":7749,"heldMapping":null}
            """
        let reading = try PeerReachReading.decode(Data(json.utf8), readAt: now)
        XCTAssertNil(reading.heldMapping)
    }

    /// A `heldMapping` object missing a field `MappingRecord` always writes
    /// (`externalPort`, `internalPort` or `expiresAtMs`) is not a mapping
    /// this app can act on: absent, not a guessed zero.
    func testHeldMappingDecodesAsAbsentWhenAFieldThisBuildNeedsIsMissing() throws {
        let json = """
            {"heldMapping":{"externalAddress":"203.0.113.44","externalPort":51413}}
            """
        let reading = try PeerReachReading.decode(Data(json.utf8), readAt: now)
        XCTAssertNil(reading.heldMapping)
    }

    /// `unavailable: no gateway` is an ABSENCE, not an address. A build that
    /// kept the string would print `Reachable at unavailable: no gateway`.
    func testUnavailableExternalAddressDecodesAsAbsent() throws {
        let json = """
            {"externalAddress":"unavailable: no gateway","mapping":"unavailable: no gateway",
             "listenPort":7749}
            """
        let reading = try PeerReachReading.decode(Data(json.utf8), readAt: now)
        XCTAssertNil(reading.externalAddress)
        XCTAssertEqual(reading.mapping, .unavailable("unavailable: no gateway"))
    }

    func testRefusalAndNotAskedAreTheirOwnOutcomes() {
        XCTAssertEqual(
            PeerReachReading.mapping("refused: unsupported opcode"),
            .refused("refused: unsupported opcode"))
        XCTAssertEqual(PeerReachReading.mapping("not asked for (pass --map)"), .notAsked)
        XCTAssertEqual(
            PeerReachReading.mapping("no peer listener is configured, so there is no port"),
            .unavailable("no peer listener is configured, so there is no port"))
    }

    /// A shape this build cannot read is a refusal carrying `tcr`'s own words,
    /// never a granted mapping: a sentence nobody parsed is not evidence a
    /// port is open.
    func testAnUnreadableMappingIsARefusalNotAGrant() {
        XCTAssertEqual(PeerReachReading.mapping("tcp who knows"), .refused("tcp who knows"))
        XCTAssertEqual(
            PeerReachReading.mapping(nil),
            .refused("tcr peer reach reported no mapping at all"))
    }

    // MARK: The four states

    func testOffDrawsNoLineAtAll() {
        let state = PeerInternetReach.state(on: false, reading: nil, now: now)
        XCTAssertEqual(state, .off)
        XCTAssertNil(state.line)
    }

    /// The press has happened and the router has not answered. This must never
    /// be the failure state: "we have not heard yet" is not "the router said
    /// no".
    func testOnWithNoReadingYetIsAsking() {
        let state = PeerInternetReach.state(on: true, reading: nil, now: now)
        XCTAssertEqual(state, .asking)
        XCTAssertEqual(state.line, "Asking your router for a way in. This takes a few seconds.")
        XCTAssertFalse(state.isWarning)
    }

    func testAGrantedMappingNamesTheExternalAddressAndPort() {
        let reading = PeerReachReading(
            externalAddress: "203.0.113.44", listenPort: 7749,
            mapping: .granted(externalPort: 51413, lifetimeSeconds: 120),
            heldMapping: .init(
                externalAddress: "203.0.113.44", externalPort: 51413, internalPort: 7749,
                expiresAtMs: Int(now.addingTimeInterval(120).timeIntervalSince1970 * 1000)),
            readAt: now)
        let state = PeerInternetReach.state(on: true, reading: reading, now: now)
        XCTAssertEqual(
            state,
            .reachable(
                address: "203.0.113.44", port: 51413,
                expires: now.addingTimeInterval(120)))
        XCTAssertEqual(state.line?.hasPrefix("Reachable at 203.0.113.44:51413."), true)
        XCTAssertFalse(state.isWarning)
    }

    /// THE rule from the lead's answers: when the held mapping's deadline is
    /// older than now, the line says the router did not answer rather than
    /// an address that has stopped working.
    func testAnExpiredMappingReadsAsRouterSilentAndNeverAsAStaleAddress() {
        let reading = PeerReachReading(
            externalAddress: "203.0.113.44", listenPort: 7749,
            mapping: .granted(externalPort: 51413, lifetimeSeconds: 120),
            heldMapping: .init(
                externalAddress: "203.0.113.44", externalPort: 51413, internalPort: 7749,
                expiresAtMs: Int(now.addingTimeInterval(120).timeIntervalSince1970 * 1000)),
            readAt: now)
        let justInside = PeerInternetReach.state(
            on: true, reading: reading, now: now.addingTimeInterval(119))
        let justPast = PeerInternetReach.state(
            on: true, reading: reading, now: now.addingTimeInterval(121))
        guard case .reachable = justInside else {
            return XCTFail("119s into a 120s mapping is still reachable, got \(justInside)")
        }
        XCTAssertEqual(justPast, .routerSilent)
        XCTAssertEqual(justPast.line?.contains("203.0.113.44"), false)
        XCTAssertEqual(justPast.line?.hasPrefix("Router did not answer."), true)
        XCTAssertTrue(justPast.isWarning)
    }

    /// No serving process holds a mapping, regardless of what this one
    /// call's own probe (`mapping`) said.
    func testNoHeldMappingIsRouterSilentEvenWithAnAddress() {
        let reading = PeerReachReading(
            externalAddress: "203.0.113.44", listenPort: 7749,
            mapping: .refused("refused: unsupported opcode"), readAt: now)
        XCTAssertEqual(
            PeerInternetReach.state(on: true, reading: reading, now: now), .routerSilent)
    }

    /// A `tcr` that could not be run at all says so in its own words. Folding
    /// it into "Router did not answer" would blame a router for a broken
    /// binary.
    func testAFailedProbeSaysSoInTcrsOwnWords() {
        let state = PeerInternetReach.unreadable("tcr: command not found")
        XCTAssertEqual(state.line, "Could not ask your router: tcr: command not found")
        XCTAssertTrue(state.isWarning)
    }

    // MARK: The retry button's own state

    /// The row's retry button only appears on the two states that ended
    /// without a path.
    func testCanRetryIsTrueOnlyOnTheTwoStatesThatEndedWithoutAPath() {
        XCTAssertTrue(PeerInternetReach.routerSilent.canRetry)
        XCTAssertTrue(PeerInternetReach.unreadable("tcr: command not found").canRetry)
        XCTAssertFalse(PeerInternetReach.off.canRetry)
        XCTAssertFalse(PeerInternetReach.asking.canRetry)
        XCTAssertFalse(PeerInternetReach.retrying.canRetry)
        XCTAssertFalse(
            PeerInternetReach.reachable(address: "203.0.113.44", port: 51413, expires: now)
                .canRetry)
    }

    /// Pressing the retry button, with the switch already on and no fresh
    /// reading yet, reads as `retrying`, not `asking`: the caller says which
    /// press it was.
    func testRetryingIsAskingsOwnTwinReachedFromTheButton() {
        let state = PeerInternetReach.state(on: true, reading: nil, retrying: true, now: now)
        XCTAssertEqual(state, .retrying)
        XCTAssertEqual(state.line, PeerInternetReach.asking.line)
        XCTAssertFalse(state.isWarning)
    }

    /// Leaving `retrying` off, the same call still reads as `asking`: the
    /// default keeps every existing caller's behaviour.
    func testStateWithoutRetryingStillReadsAsAsking() {
        XCTAssertEqual(PeerInternetReach.state(on: true, reading: nil, now: now), .asking)
    }

    // MARK: Words

    /// The jargon rule: no protocol name reaches a string an
    /// operator reads.
    func testNoJargonInAnyLineTheOperatorReads() {
        let lines: [String] = [
            PeerInternetReach.asking.line,
            PeerInternetReach.routerSilent.line,
            PeerInternetReach.reachable(
                address: "203.0.113.44", port: 51413, expires: now
            ).line,
            PeerInternetReach.unreadable("no").line,
            PeerInternetReach.rowDetail(on: true),
            PeerInternetReach.rowDetail(on: false),
        ].compactMap { $0 }
        XCTAssertEqual(lines.count, 6)
        for line in lines {
            for word in ["NAT-PMP", "UPnP", "egress", "IK"] {
                XCTAssertFalse(
                    line.contains(word), "\(word) reached a user-visible line: \(line)")
            }
        }
    }

    /// The row's own sub-line changes with the switch, because "off" and "on"
    /// mean two different things about who can reach this Mac.
    func testRowDetailChangesWithTheSwitch() {
        XCTAssertNotEqual(
            PeerInternetReach.rowDetail(on: true), PeerInternetReach.rowDetail(on: false))
        XCTAssertTrue(PeerInternetReach.rowDetail(on: false).hasPrefix("Off:"))
        XCTAssertTrue(PeerInternetReach.rowDetail(on: true).hasPrefix("On:"))
    }

    // MARK: The wire field

    /// `internet` is OPTIONAL on the document: the `tcr` in this tree writes
    /// none of the This Mac readouts, and a `false` default would be this
    /// panel asserting a setting nobody reported.
    func testInternetDecodesAsAbsentWhenTheProducerIsSilent() throws {
        let absent = try JSONDecoder().decode(
            PeerListDocument.self, from: Data("{\"supported\":true}".utf8))
        XCTAssertNil(absent.internet)
        let present = try JSONDecoder().decode(
            PeerListDocument.self, from: Data("{\"internet\":true}".utf8))
        XCTAssertEqual(present.internet, true)
    }
}
