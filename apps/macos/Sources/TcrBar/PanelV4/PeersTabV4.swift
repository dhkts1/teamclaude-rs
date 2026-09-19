import Foundation
import SwiftUI
import TcrBarCore

// MARK: - What the tab reads
//
// `PeerListDocument` (the document `tcr peer ls --json` answers with) moved
// to `TcrBarCore/PeerListDocument.swift`. It is pure `Decodable`
// data, and while it sat here every assertion about it had to read this file as
// TEXT: `Package.swift:39-43` gives the test target `TcrBarCore` alone. The
// four blocks decision rows 10 to 13 added (`pending`, `blocked`, `muted`,
// `caps`, and per-account `lentTo`) are nested shapes whose key names have to
// match `src/main.rs`'s `PeerLsJson`, and the only honest gate for that is
// decoding the real JSON, which is now `PeerListDocumentTests`.

// MARK: - A row

/// One Mac on the tab, with everything the row draws already decided.
///
/// A view model rather than the wire row, because two of the row's strings are
/// not in the wire at all: the freshness phrase is a function of `lastSeenMs`
/// and now, and every "what yes does" sentence is this panel's own words. The
/// view renders; it does not compute.
struct PeerRowModel: Identifiable, Equatable {
    enum Trust: Equatable {
        /// Found, not trusted. Carries the address `Trust` dials and the
        /// sentence that says what pressing it buys.
        ///
        /// The ADDRESS and not the argv it used to hold. Pairing is now a
        /// held-open process whose argv is `PeerCommand.pairJSON(address:)`
        /// and whose stdin this panel writes (``PeerPairRun``), so a row
        /// carrying a pre-built `["peer", "pair", addr]` was carrying the one
        /// spelling that cannot finish: it blocks on a `read_line` nothing can
        /// answer. One value, built into argv in one place.
        case found(dialAddress: String, promise: String)
        /// Trust was pressed, the knock is away, and the other operator has
        /// not answered.
        ///
        /// A THIRD state rather than a button that greys for the length of a
        /// subprocess. Decision row 10 makes pairing two phases: pressing
        /// Trust SENDS a knock and nothing else happens until somebody on the
        /// other Mac presses Accept, which can be an hour later, or never.
        /// With two states the row was byte-identical before and after the
        /// press, so the one thing the operator needed to know (it is sent,
        /// now wait) was the one thing the tab could not say.
        ///
        /// Carries the address because that is what the row is keyed on and
        /// what Cancel has to clear.
        case waiting(address: String)
        case trusted
    }

    /// The row's list identity. The peer id once trusted; the address before
    /// that, because an untrusted Mac has no id to key on (decision row 9).
    let id: String
    /// The name, or the address when no name was announced.
    let title: String
    /// Where this Mac answers, when the wire named it.
    ///
    /// Kept beside ``title`` rather than derived from it: on a trusted row the
    /// title is the NAME and ``id`` is the peer id, and `tcr peer block` takes
    /// neither. It takes an address, so the row has to carry one to be able to
    /// offer Block at all.
    let address: String?
    /// Whether ``title`` is an address and should be drawn in mono.
    let titleIsAddress: Bool
    let trust: Trust
    /// `found 2s ago · not trusted`, or `awake` / `last seen 6m ago`.
    let freshness: String
    /// Whether the freshness reads as present. Drives the dot, never the text.
    let awake: Bool
    let pills: [(text: String, role: PeerPill.Role)]
    /// Whether this Mac holds the CARRY grant: it may hold this Mac's
    /// encrypted bytes and can open none of them.
    let carries: Bool
    /// Whether it holds the SERVE grant, the plaintext one. Read straight
    /// off the wire rather than inferred from which meter the row drew: a
    /// grant is a fact about the peers file, and deriving it from a rendering
    /// decision would make Settings disagree with the tab the moment a meter
    /// changed shape.
    let serves: Bool
    /// Decision row 12's leases, as the LENDER recorded them, one per scope.
    /// Empty on a Mac with no lease, and empty against a `tcr` that does not
    /// send `lend` yet, which is the state the per-Mac sheet draws as its
    /// "Add a lease…" row alone.
    let lend: [PeerLendGrant]
    /// One line per way this Mac knows to reach that one, already worded by
    /// ``PeerFormat/pathLine(_:)``. Empty on an untrusted row, and empty
    /// against a `tcr` whose live half this build could not read, in which
    /// case the row draws exactly what it drew before the paths existed.
    let pathLines: [PeerPathLine]
    /// The first path's own figures, kept beside the worded lines for the mini
    /// mesh: the card draws a dot and a number, not a sentence, and re-parsing
    /// them out of `pathLines` would be a second place the wording matters.
    var pathRttMs: Double? = nil
    var pathLossPct: Double? = nil
    /// The Mac carrying this one's bytes, already named rather than left as
    /// the peer id the wire sends.
    var pathViaName: String? = nil
    let meter: PeerMeter

    /// What this row DRAWS, for the height budget.
    ///
    /// Derived from the row rather than from its meter, which is the fix for a
    /// real hazard rather than tidiness: ``PeersSnapshot/rowShapes`` used to
    /// ask the METER how many sub lines the row has, so a path line added
    /// under it grew the drawn row while the budget's arithmetic stayed put,
    /// and growth the budget cannot see comes out of the footer
    /// (``PeerPanelHeight``'s own invariant).
    var rowShape: PeerPanelHeight.Row {
        let base = meter.rowShape
        return PeerPanelHeight.Row(
            subLines: base.subLines + pathLines.count, hasMeter: base.hasMeter)
    }

    static func == (lhs: PeerRowModel, rhs: PeerRowModel) -> Bool {
        lhs.id == rhs.id && lhs.title == rhs.title && lhs.titleIsAddress == rhs.titleIsAddress
            && lhs.trust == rhs.trust && lhs.freshness == rhs.freshness && lhs.awake == rhs.awake
            && lhs.pills.map(\.text) == rhs.pills.map(\.text) && lhs.carries == rhs.carries
            && lhs.serves == rhs.serves && lhs.lend == rhs.lend && lhs.meter == rhs.meter
            && lhs.pathLines == rhs.pathLines
    }

    /// The same row, once its knock is away.
    ///
    /// A rewrite of the row rather than a flag on it, so the waiting state
    /// cannot be drawn half-applied: the trust arm, the sub-line and the
    /// meter move together. The row keeps its identity and its name, and the
    /// shape it draws is unchanged, a name line and one sub line, so the
    /// list's height budget (``PeerPanelHeight``) does not move when a knock
    /// goes out.
    func waitingForAccept() -> PeerRowModel {
        PeerRowModel(
            id: id,
            title: title,
            address: address,
            titleIsAddress: titleIsAddress,
            trust: .waiting(address: id),
            freshness: PeerAdmission.waitingLine(name: title),
            awake: awake,
            pills: [],
            carries: false,
            serves: false,
            lend: [],
            // No path line while a knock is out: nothing is pinned yet, so
            // this Mac knows no way to reach that one that it may use.
            pathLines: [],
            meter: .none(nil))
    }
}

/// Everything the tab draws, derived once from the document.
///
/// Derived in ONE place so the footer count, the Share switch's enabled state
/// and the section head's pill cannot disagree about the same fleet of Macs:
/// the three of them are three readings of the same two numbers.
struct PeersSnapshot: Equatable {
    /// An older `tcr` said `{"supported": false}`, or the subcommand is
    /// missing. The tab collapses to one honest line.
    let unsupported: Bool
    /// `tcr` could not be reached or refused. Its own words, unparaphrased.
    let failure: String?
    let finding: Bool
    let sharing: Bool
    /// `var` for ONE writer: ``PeersSnapshotBuilder/waiting(_:knocked:)``,
    /// which re-states the rows this panel has knocked at. Nothing else
    /// mutates a snapshot after it is derived.
    var rows: [PeerRowModel]
    let answeringOn: PeerListDocument.AnsweringOn?
    /// The instant this snapshot was read AT, carried so that anything the
    /// tab counts counts from the same clock the rest of the snapshot was
    /// worded against.
    ///
    /// The row ages (`found 2s ago`) are already phrases decided at read time;
    /// a deadline is not, because it is a count DOWN and the view is what
    /// draws it. Reading `Date()` inside the view instead would make a
    /// fixture drawn by `--render-states` count against the real clock, so
    /// one scene's PNG would differ from the last run's for no design reason.
    var readAt: Date = Date()
    /// The five Settings-only readouts, straight off the document. `nil` is
    /// "not read yet", which is what the pane draws.
    var name: String? = nil
    var announceName: Bool? = nil
    var nodeId: String? = nil
    var listenAddress: String? = nil
    var via: String? = nil
    var maxHops: Int? = nil
    /// Whether this Mac is asking its router to be reachable from off this
    /// network. `nil` is "not read yet" and the switch draws the shipped
    /// default, off.
    var internet: Bool? = nil

    // MARK: Decision rows 10 to 13
    //
    // Carried on the snapshot rather than re-read per view, for the reason the
    // type's own header gives: the footer's count, the pending rows above it
    // and the Blocked list one pane down are three readings of ONE document,
    // and two reads would see two instants.

    /// Macs asking to connect. Nothing is pinned, carried or served until the
    /// operator answers one of these.
    var pending: [PeerKnock] = []
    /// `pendingCount` as the producer counted it, which is not always
    /// `pending.count`.
    var pendingCount: Int = 0
    var blocked: [PeerBan] = []
    var muted: [PeerMute] = []
    /// Found rows this Mac is holding back, the "N more not shown" footer.
    var limited: Int = 0
    /// The caps the binary enforces. `nil` from a `tcr` that predates them.
    var caps: PeerCaps? = nil
    /// Who is drawing on each account label, for the Accounts tab's own
    /// "Lent to …" line.
    var lentTo: [String: [PeerLentToEntry]] = [:]
    /// Where each account's requests leave from, keyed by account label.
    /// Empty against every `tcr` in this tree, which draws no "Exits from"
    /// row at all.
    var exits: [String: AccountExit] = [:]
    /// This Mac's own name, for the mini mesh's root tile. `nil` is "not read
    /// yet", which the card draws as `This Mac` rather than a guess.
    var thisMac: String? = nil
    /// The document's own peer rows, kept beside the view models built from
    /// them.
    ///
    /// ``rows`` is what this tab draws and it has already thrown away the
    /// pinned id; the Accounts tab needs it, because an exit lock names a Mac
    /// by that id and the picker has to show the operator's word for it.
    /// Passing the rows themselves rather than a second id-to-name map: one
    /// already exists here (``peerNames``) for the forwarded-path line, and a
    /// second copy of the same join is how the two end up disagreeing about a
    /// Mac with no name yet.
    var entries: [PeerListDocument.PeerEntry] = []

    /// `12 shown, 3 more not shown`, or `nil` when nothing was held back.
    var limitedFooter: String? {
        PeerAdmission.limitedFooter(shown: rows.count, limited: limited)
    }

    /// The trusted Macs as the mini mesh needs them: one per tile, with the
    /// first path's own figures, because the first path is the one a dial
    /// tries first and the card has room for one reading per Mac.
    var meshPeers: [PeerMeshPeer] {
        rows.filter { $0.trust == .trusted }.map { row in
            PeerMeshPeer(
                name: row.title, rttMs: row.pathRttMs, lossPct: row.pathLossPct,
                viaName: row.pathViaName, asleep: !row.awake)
        }
    }

    var trustedCount: Int { rows.filter { $0.trust == .trusted }.count }
    var foundCount: Int { rows.count }
    /// The Share switch is disabled until a Mac is trusted: sharing with
    /// nobody is not a state worth entering, and a live switch that does
    /// nothing is worse than a dim one.
    var canShare: Bool { trustedCount > 0 }

    /// `2 Macs found, 1 trusted`.
    var countLine: String {
        let macs = foundCount == 1 ? "1 Mac found" : "\(foundCount) Macs found"
        return "\(macs), \(trustedCount) trusted"
    }

    static let empty = PeersSnapshot(
        unsupported: false, failure: nil, finding: false, sharing: false, rows: [],
        answeringOn: nil)

    /// The rows' shapes, for ``PeerPanelHeight``.
    var rowShapes: [PeerPanelHeight.Row] { rows.map(\.rowShape) }
}

// MARK: - Deriving the snapshot

/// Turning `tcr peer ls --json` into ``PeersSnapshot``: the freshness phrases,
/// the pills, the meters and every "what yes does" sentence.
///
/// A separate type from the view so the sentences are readable in one place.
/// Every one of them is held to the 2026-09-12 approval-card rule the mockup
/// restates as rule 9: a card with a control closes with what yes does, in one
/// line, inside the card. And to rule 1: a grant that lets another Mac read
/// plaintext names both parties, a direction and a verb, and the word `read`
/// never appears alone.
enum PeersSnapshotBuilder {
    /// The carry pill's word, and it names a CAPABILITY.
    ///
    /// It said `carries`, which reads as "is relaying right now", on a row
    /// whose own path line said that Mac's traffic goes through a third one.
    /// The grant is permission to relay; whether anything is being relayed at
    /// this instant is what the path line under the pill answers. Written once
    /// because the pill and the sentence behind it (``PeersTabV4/pillHelp(_:)``)
    /// are the same string in two places, and the copy that drifts is the one
    /// nobody reads.
    static let carryPillText = "can carry"

    /// What carrying means, said ONCE under the section head.
    ///
    /// It was a per-row sentence, repeated word for word on every trusted row:
    /// seven identical paragraphs in the seven-Mac state, for a fact that is
    /// true of every trusted Mac and changes for none of them. A row keeps a
    /// sentence of its own only where it DEVIATES, which is the carrying row
    /// with its own byte figures.
    ///
    /// `path`, not `route`: one noun for a way to reach a Mac, on the whole
    /// tab.
    static let carrySentence =
        "A trusted Mac carries your traffic when this Mac has no path of its own, and reads "
        + "none of it."

    static func snapshot(from document: PeerListDocument, now: Date) -> PeersSnapshot {
        guard document.supported else {
            return PeersSnapshot(
                unsupported: true, failure: nil, finding: false, sharing: false, rows: [],
                answeringOn: nil)
        }
        return PeersSnapshot(
            unsupported: false,
            failure: nil,
            finding: document.finding,
            sharing: document.sharing,
            rows: document.peers.map {
                row($0, sharing: document.sharing, now: now, names: peerNames(document))
            },
            answeringOn: document.answeringOn,
            readAt: now,
            name: document.name,
            announceName: document.announceName,
            nodeId: document.nodeId,
            listenAddress: document.listenAddress,
            via: document.via,
            maxHops: document.maxHops,
            internet: document.internet,
            pending: document.pending,
            pendingCount: document.pendingCount,
            blocked: document.blocked,
            muted: document.muted,
            limited: document.limited,
            caps: document.caps,
            lentTo: document.lentTo,
            exits: document.exits,
            thisMac: document.name,
            entries: document.peers)
    }

    /// Peer id to the name the operator knows, for a forwarded path's own
    /// line: the wire names a forwarder by id, and an operator reading
    /// `via tcr-4b8we1r0zp` where every other row shows a name has to go and
    /// look it up. An id with no name keeps the id.
    private static func peerNames(_ document: PeerListDocument) -> [String: String] {
        var names: [String: String] = [:]
        for peer in document.peers {
            guard let id = peer.id, let name = peer.name else { continue }
            names[id] = name
        }
        return names
    }

    /// The same snapshot with every found row this panel has knocked at moved
    /// to ``PeerRowModel/Trust/waiting(address:)``.
    ///
    /// # Why the waiting state is this panel's own memory and not a field
    ///
    /// `tcr peer ls --json` reports what the PEERS FILE holds, and a knock
    /// this Mac sent leaves nothing in it: decision row 10's phase 1 reveals
    /// no static key and pins nothing, so there is no row to read back. The
    /// one process that knows the request went out is the one that ran
    /// `tcr peer pair`, and that is this panel.
    ///
    /// What that costs, stated rather than hidden: the memory dies with the
    /// panel, so a relaunch draws the row as found again while the request is
    /// still standing on the other Mac. A `pairing` field on the wire would
    /// survive it, and a follow-up should add one. Nothing here invents a
    /// second spelling in the meantime.
    ///
    /// Applied on the way OUT, over a snapshot kept as it was read, so
    /// dropping an address from the set restores the found row exactly.
    static func waiting(_ snapshot: PeersSnapshot, knocked: Set<String>) -> PeersSnapshot {
        guard !knocked.isEmpty else { return snapshot }
        var out = snapshot
        out.rows = snapshot.rows.map { row in
            guard case .found = row.trust, knocked.contains(row.id) else { return row }
            return row.waitingForAccept()
        }
        return out
    }

    static func failed(_ message: String) -> PeersSnapshot {
        PeersSnapshot(
            unsupported: false, failure: message, finding: false, sharing: false, rows: [],
            answeringOn: nil)
    }

    private static func row(
        _ entry: PeerListDocument.PeerEntry, sharing: Bool, now: Date,
        names: [String: String] = [:]
    ) -> PeerRowModel {
        let address = entry.address ?? entry.name ?? "unknown"
        let named = entry.name != nil
        let title = entry.name ?? address
        let identity = entry.trusted ? (entry.id ?? address) : address
        let seen = PeerFormat.sinceLastSeen(entry.lastSeenMs, now: now)

        guard entry.trusted else {
            let detail =
                named
                ? "found \(seen.phrase) · not trusted"
                : "found \(seen.phrase) · not trusted · no name announced"
            let promise =
                named
                ? "Trust shows the same six digits here and on \(title). Press Trust on both "
                    + "and it can carry your traffic without reading it."
                : "This Mac's owner turned announcing its name off, so it shows an address "
                    + "until you trust it. Trust still compares six digits, and the name "
                    + "arrives with the handshake."
            return PeerRowModel(
                id: identity,
                title: title,
                address: entry.address,
                titleIsAddress: !named,
                trust: .found(dialAddress: address, promise: promise),
                freshness: detail,
                awake: seen.awake,
                pills: [],
                carries: false,
                serves: false,
                lend: [],
                pathLines: [],
                meter: .none(nil))
        }

        // ONE meter, built once and asked twice: the pills have to agree with
        // it about whether the lease is over, and deriving "ended" a second
        // time is how the row ends up with a live pill over a dead meter.
        let meter = meter(entry, title: title, sharing: sharing, awake: seen.awake, now: now)
        return PeerRowModel(
            id: identity,
            title: title,
            address: entry.address,
            titleIsAddress: !named,
            trust: .trusted,
            freshness: seen.awake ? "awake" : "last seen \(seen.phrase)",
            awake: seen.awake,
            pills: pills(entry, awake: seen.awake, ended: meter.isEnded),
            carries: entry.carries,
            serves: entry.serves,
            lend: entry.lend,
            // Newest first, as the serving process ordered them: the first
            // line is the path a dial would try first.
            pathLines: PeerFormat.pathLines(entry.paths, names: names),
            pathRttMs: entry.paths.first?.rttMs,
            pathLossPct: entry.paths.first?.lossPct,
            pathViaName: entry.paths.first.flatMap { path in
                guard case .via = path.kind else { return nil }
                return names[path.endpoint] ?? path.endpoint
            },
            meter: meter)
    }

    private static func pills(
        _ entry: PeerListDocument.PeerEntry, awake: Bool, ended: Bool
    ) -> [(text: String, role: PeerPill.Role)] {
        // The first pill says `trusted`, never `awake`. There was no ruling on
        // this before; here is one, and why.
        //
        // `trusted` measures PAIRING: a static fact this build reads off
        // `entry.trusted`, true from the moment both sides pressed Trust
        // until Forget, independent of whether that Mac is reachable right
        // now. `awake` measures the LAST PROBE: whether `tcr status --json`
        // heard back inside the freshness window (`seen.awake`, passed in
        // below). `wave12-ui-mockup.html`'s per-Mac sheet header draws
        // `studio-mac awake` as its own pill and states pairing in the
        // subtitle instead (`tcr-4b8we1r0zp · trusted since 12 September`), a
        // sheet with room for a second line can afford to make freshness
        // the headline and pairing the footnote. This row has one line and
        // no footnote, and pairing is the fact an operator reaches for first:
        // it is what decides whether Trust or Forget is even offered, while
        // asleep already gets its own pill a few lines down when it is the
        // whole story. So the WORD stays `awake` for freshness on both
        // surfaces (the row states it in the freshness readout beside the
        // pill, `seen.awake ? "awake" : "last seen …"`, line 397) and `trusted`
        // for pairing on both (this pill here, the sheet's subtitle there);
        // only which one gets the pill differs, and that follows from which
        // fact the surface has room to lead with.
        //
        // TWO pills, never three, and that is a layout rule rather than a
        // taste one. `V4Row` gives its trailing column `layoutPriority(1)` and
        // clips the leading label (its own doc-comment says so), so a third
        // pill beside the freshness readout takes the row's whole width and
        // the peer's NAME (the one string the row is for) renders as zero
        // characters. Found by opening `50-peers-borrowing-dark.png`, not by
        // a green build: every gate passed with attic-nuc's name gone.
        //
        // The mockup's own maximum is two, and it picks the same two: a row
        // with nothing spare says `trusted` and `no headroom`, and the grant
        // pill steps aside, because what an operator does about a lender with
        // no headroom does not depend on which grant it holds.
        var pills: [(text: String, role: PeerPill.Role)] = [("trusted", .ok)]
        // An ended lease takes the second pill, ahead of every other state:
        // the row kept its SERVES pill after its only lease expired, which
        // told the operator a grant was live when it was over. Neutral, not
        // the reserved plaintext hue, because nothing is being read any more.
        if ended {
            pills.append(("lease ended", .neutral))
            return pills
        }
        if entry.noHeadroom {
            pills.append(("no headroom", .neutral))
            return pills
        }
        if !awake {
            pills.append(("asleep", .neutral))
            return pills
        }
        if (entry.inFlight ?? 0) > 0 {
            // The disclosure pill, in the reserved hue. The direction is what
            // the operator needs, said with the same word the meter below
            // uses (`PeerLendDirection`), so the two cannot disagree.
            pills.append((PeerLendDirection.theyLend.pillText, .disclosure))
        } else if entry.serves {
            pills.append((PeerLendDirection.youLend.pillText, .disclosure))
        } else if entry.carries {
            pills.append((carryPillText, .info))
        }
        return pills
    }

    /// Which meter a row gets, and therefore which unit it is allowed to
    /// speak in. A serving row gets a fraction of an allowance; a
    /// carry-only row gets bytes per hour; anything else gets a sentence.
    private static func meter(
        _ entry: PeerListDocument.PeerEntry, title: String, sharing: Bool, awake: Bool,
        now: Date
    ) -> PeerMeter {
        // The ended arm goes FIRST, because every arm below it describes a
        // lease that is running. A Mac whose only lease had ended kept its
        // meter and the sentence "the offer stands and starts again by
        // itself", which is the opposite of what had happened.
        if let ended = LeaseEnded.forEntry(entry, title: title, now: now) {
            return .ended(ended)
        }
        if entry.noHeadroom {
            return .none("\(title) has nothing spare right now, so nothing was asked of it.")
        }
        if entry.serves, let spent = entry.leaseSpent {
            // Decision row 13's end, said ONCE and appended to whichever of
            // the four sentences below the row is drawing. Written four times
            // it would be four chances to forget it, and forgetting it is the
            // blocker: on the scene where this Mac has nothing spare and
            // depends entirely on the lender, the work stopped at an hour the
            // screen never named.
            let ends = entry.endsInSentence(now: now).map { " " + $0 } ?? ""
            let inFlight = entry.inFlight ?? 0
            // The same direction the pill above picked, from the same fact
            // (`inFlight`), so the meter label and the pill cannot disagree.
            let direction: PeerLendDirection = inFlight > 0 ? .theyLend : .youLend
            guard awake else {
                // Rule 5: a Mac that has stopped serving renders zero, never
                // its last value, and the offer standing is a different field
                // from the spend so the row can say both.
                return .lease(
                    LeaseFraction(
                        spent: 0,
                        sentence: "Nothing is being served while it is away, so this reads "
                            + "zero. The offer stands and starts again by itself when "
                            + "\(title) wakes." + ends,
                        label: direction.meterLabel))
            }
            if inFlight > 0 {
                return .lease(
                    LeaseFraction(
                        spent: spent,
                        sentence: "\(PeerFormat.requests(inFlight)) on \(title)'s accounts now, "
                            + "and it has spent \(PeerFormat.share(spent)) of what it offered "
                            + "you. It reads what it serves, and your sign-in stays here."
                            + ends,
                        label: direction.meterLabel))
            }
            if spent <= 0 {
                return .lease(
                    LeaseFraction(
                        spent: 0,
                        sentence: "\(title) has not served a request yet. The same offer "
                            + "stands as for every trusted Mac." + ends,
                        label: direction.meterLabel))
            }
            return .lease(
                LeaseFraction(
                    spent: spent,
                    sentence: "\(title) has used \(PeerFormat.share(spent)) of what you offered "
                        + "it this week"
                        + PeerFormat.ttlClause(entry.leaseTtlSeconds) + "." + ends,
                    label: direction.meterLabel))
        }
        if entry.carries, let bytes = entry.bytesPerHour, let cap = entry.byteCapPerHour {
            return .gateway(
                GatewayBytes(
                    bytesPerHour: bytes,
                    capBytesPerHour: cap,
                    sentence: "\(title) is carrying your encrypted bytes and can open none of "
                        + "them, out of \(PeerFormat.megabytes(cap)) MB an hour."))
        }
        if entry.carries {
            // No sentence: this row says exactly what ``carrySentence`` says
            // above the list, so it says nothing and the reader reads it once.
            return .none(nil)
        }
        return .none(nil)
    }

}

// MARK: - Reading it

/// What one verb printed, for a control whose output is the thing the
/// operator came for.
///
/// Two cases and no third: either `tcr` printed something, or this is the
/// sentence to put on screen. There is no "ran, and here is the empty string"
///, a sheet that drew a blank where a join key belongs is a silent fallback
/// with a spinner on it.
enum PeerCapture: Equatable {
    case text(String)
    case failed(String)
}

/// Reads `tcr peer ls --json` on the panel's own cadence and runs
/// `tcr peer <verb>` for the two switches and Trust.
///
/// Subprocess-only, and that is the whole architecture of this tab: no socket,
/// no crypto, no new Swift dependency (Sparkle stays the only one). `TcrBar`
/// learns nothing about Noise, and a peer cannot reach this process at all.
///
/// ``pinned(_:)`` is the harness's door in, the same shape
/// `StatusPoller.init(pinnedState:)` and `ServerController.harness(pinned:)`
/// already use: `--render-states` draws fixture peers without a proxy, a
/// listener or a subprocess of any kind.
@MainActor
final class PeerController: ObservableObject {
    @Published private(set) var snapshot: PeersSnapshot
    /// Verbs in flight, by argv, so a switch cannot be pressed twice into a
    /// race with itself.
    @Published private(set) var pending: Set<String> = []

    /// Addresses this panel has sent a knock to, in this session.
    ///
    /// Not a cache of anything `tcr` knows: a sent knock leaves no row in the
    /// peers file (decision row 10, phase 1), so this process is the only
    /// place the fact exists. ``PeersSnapshotBuilder/waiting(_:knocked:)``
    /// carries the whole reasoning, including what it costs at relaunch.
    @Published private(set) var knocked: Set<String> = []

    /// The last verb `tcr` refused, if it has not been answered yet.
    ///
    /// Beside the snapshot and not in it, which is the whole of
    /// ``PeerRefusal``'s reasoning: a refusal written into the snapshot was
    /// drawn INSTEAD of the tab and then wiped by the next poll about three
    /// seconds later.
    @Published private(set) var refusal = PeerRefusal()

    /// The snapshot exactly as it was read, before the waiting overlay.
    ///
    /// Kept so that clearing an address from ``knocked`` restores the found
    /// row rather than leaving a row this panel rewrote: a one-way transform
    /// applied in place would make Cancel unable to undo itself.
    private var readSnapshot: PeersSnapshot

    private let interval: TimeInterval
    private let isPinned: Bool
    private var task: Task<Void, Never>?

    init(interval: TimeInterval = StatusPoller.defaultInterval) {
        self.interval = interval
        self.isPinned = false
        self.snapshot = .empty
        self.readSnapshot = .empty
    }

    private init(pinned: PeersSnapshot, refusal: PeerRefusal) {
        self.interval = StatusPoller.defaultInterval
        self.isPinned = true
        self.snapshot = pinned
        self.readSnapshot = pinned
        self.refusal = refusal
    }

    /// A controller that reads nothing, runs nothing and answers `pinned`
    /// forever.
    ///
    /// `refusal` is how the harness draws the banner: a refused verb has no
    /// fixture otherwise, because nothing in a render run can press a button.
    static func pinned(_ snapshot: PeersSnapshot, refusal: PeerRefusal = PeerRefusal())
        -> PeerController
    {
        PeerController(pinned: snapshot, refusal: refusal)
    }

    func start() {
        guard !isPinned, task == nil else { return }
        task = Task { [weak self] in
            while !Task.isCancelled {
                await self?.refresh()
                guard let interval = self?.interval else { return }
                try? await Task.sleep(nanoseconds: UInt64(interval * 1_000_000_000))
            }
        }
    }

    func stop() {
        task?.cancel()
        task = nil
    }

    func refresh() async {
        guard !isPinned else { return }
        let read = await Task.detached(priority: .userInitiated) { Self.read() }.value
        readSnapshot = read
        publish()
    }

    /// Trust, on a found row: start the pairing AND remember that it went.
    ///
    /// One method rather than a start beside a `knocked.insert` at the call
    /// site, because the two halves are one act: a press that ran the verb and
    /// forgot to record it draws a row that has not changed, which is the
    /// defect the waiting state exists to fix.
    ///
    /// # Why this returns a run and no longer calls ``run(_:)``
    ///
    /// It used to be `run(PeerCommand.pair(address:))`: exec, wait, read the
    /// exit code. That is the one spelling of this verb that cannot finish.
    /// `tcr peer pair` holds a live handshake, prints six digits, and then
    /// blocks reading the OTHER Mac's six digits off its stdin, which
    /// ``TcrTool/run(executable:arguments:stdin:)`` does not give it. The
    /// press therefore left a subprocess sitting for ten minutes and a sheet
    /// whose Trust button was disabled forever.
    ///
    /// `nil` when `tcr` cannot be found, with the reason already on the tab
    /// through ``report(failure:)``: a sheet over a binary that is not there
    /// would have nothing to draw and no way to say why.
    func startPairing(rowId: String, dialAddress: String) -> PeerPairRun? {
        guard !isPinned else { return nil }
        switch TcrTool.resolve() {
        case .failure(let notFound):
            report(
                failure: "tcr not found (searched \(notFound.searched.count) locations). "
                    + TcrTool.overrideRemedy)
            return nil
        case .success(let executable):
            knocked.insert(rowId)
            publish()
            return PeerPairRun(address: dialAddress, executable: executable)
        }
    }

    /// Cancel, on a waiting row. Stops this panel waiting; the request itself
    /// stands on the other Mac until it is answered or expires
    /// (``PeerAdmission/cancelWaitingHelp`` says exactly that, because there
    /// is no verb that withdraws a knock).
    func stopWaiting(address: String) {
        guard knocked.remove(address) != nil else { return }
        publish()
    }

    /// The one place ``snapshot`` is assigned from a read: what `tcr` said,
    /// plus this panel's own waiting rows.
    private func publish() {
        snapshot = PeersSnapshotBuilder.waiting(readSnapshot, knocked: knocked)
    }

    /// Runs one verb, then re-reads. The re-read is what makes the switch
    /// honest: the panel shows what `tcr` says the state is, never what the
    /// press assumed it would become.
    func run(_ arguments: [String]) {
        run(arguments: arguments, stdin: nil)
    }

    /// The same run, for a verb whose input is a secret: the argv goes to
    /// `exec` and the secret goes down a pipe
    /// (``PeerSecretInvocation``, `tcr peer join --stdin`).
    ///
    /// It takes the INVOCATION rather than a string and a key, so this method
    /// cannot be handed the secret as an argument, and the in-flight key and
    /// any failure message are built from ``PeerSecretInvocation/arguments``
    /// alone, which is why neither a duplicate press nor a refused join can
    /// put the key on screen or in a log line.
    func run(_ invocation: PeerSecretInvocation) {
        run(arguments: invocation.arguments, stdin: invocation.stdin)
    }

    private func run(arguments: [String], stdin: String?) {
        guard !isPinned else { return }
        let key = arguments.joined(separator: " ")
        guard !pending.contains(key) else { return }
        pending.insert(key)
        Task { [weak self] in
            let outcome = await Task.detached(priority: .userInitiated) {
                Self.perform(arguments: arguments, stdin: stdin)
            }.value
            guard let self else { return }
            self.pending.remove(key)
            if case .failed(let message) = outcome {
                // No silent fallback: `tcr`'s own words, on the tab, rather
                // than a press that looks like it worked. In the refusal
                // beside the snapshot and not in the snapshot itself, so the
                // tab keeps every control it had and the next poll cannot wipe
                // the sentence three seconds later (``PeerRefusal``).
                self.refusal.refused(message)
                return
            }
            // The press did what it said. Whatever refusal was on screen is
            // answered by that, which is one of the only two ways it goes.
            self.refusal.succeeded()
            await self.refresh()
        }
    }

    /// Runs one verb and hands back what it PRINTED, for the one control
    /// whose whole output is the point: `tcr peer invite` answers with the
    /// join key on stdout, and a fire-and-forget `run(_:)` throws it away.
    ///
    /// No re-read afterwards, unlike ``run(_:)``: an invite mints a token and
    /// changes nothing `tcr peer ls --json` reports, so a poll here would be a
    /// subprocess that cannot alter what is on screen. A failure arrives as
    /// ``PeerCapture/failed(_:)`` and the caller shows it, the pane never
    /// draws an empty sheet where a key should be.
    func capture(_ arguments: [String], into sink: @escaping (PeerCapture) -> Void) {
        // A pinned controller runs nothing at all, the same guard `run(_:)`
        // and `refresh()` carry: `--render-settings` exists to write PNGs
        // without a subprocess, and nothing in a render run can press a
        // button anyway.
        guard !isPinned else { return }
        let key = arguments.joined(separator: " ")
        guard !pending.contains(key) else { return }
        pending.insert(key)
        Task { [weak self] in
            let captured = await Task.detached(priority: .userInitiated) {
                Self.capture(arguments: arguments)
            }.value
            guard let self else { return }
            self.pending.remove(key)
            sink(captured)
        }
    }

    /// Put a failure this panel produced OUTSIDE a verb onto the tab, in the
    /// same place `tcr`'s own refusals land. One writer, so a message cannot
    /// be drawn in two different ways.
    func report(failure: String) {
        refusal.refused(failure)
    }

    /// The operator read the banner. The other way it goes is a verb that
    /// succeeds.
    func dismissRefusal() {
        refusal.dismissed()
    }

    func isPending(_ arguments: [String]) -> Bool {
        pending.contains(arguments.joined(separator: " "))
    }

    /// Not `private`: ``perform(arguments:stdin:)`` returns it and the URL
    /// handler's ``join(link:)`` runs through that same one path, so a link
    /// and a pasted key cannot be invoked two different ways.
    enum Outcome {
        case clean
        case failed(String)
    }

    /// Run a whole `tcr://` link, for the app's URL handler.
    ///
    /// Blocking, and `nonisolated` so the handler can call it off the main
    /// actor: a join runs a subprocess and the app must not stall on it.
    ///
    /// Static, with no controller involved, because a link can arrive before
    /// any panel exists, opening one in a chat window LAUNCHES this app. The
    /// answer is a sentence for the log, never the link: whichever way it
    /// went, the string that arrived is a credential.
    ///
    /// `replace` is the confirmation sheet's own answer (the operator
    /// confirmed a Join that the sheet told them would overwrite an existing
    /// network key): it becomes `--replace` on argv, the flag `tcr peer
    /// join` requires before it will take that overwrite.
    nonisolated static func join(link: URL, replace: Bool = false) -> String {
        switch PeerCommand.join(link: link, replace: replace) {
        case .failure(let refusal):
            return PeerJoinLink.sentence(for: refusal)
        case .success(let invocation):
            switch perform(arguments: invocation.arguments, stdin: invocation.stdin) {
            case .clean:
                return "joined"
            case .failed(let message):
                return message
            }
        }
    }

    /// Blocking, always called off the main actor. Stdout on success,
    /// `tcr`'s own words on failure, never a plausible-looking empty string.
    private nonisolated static func capture(arguments: [String]) -> PeerCapture {
        switch TcrTool.resolve() {
        case .failure(let notFound):
            return .failed(
                "tcr not found (searched \(notFound.searched.count) locations). "
                    + TcrTool.overrideRemedy)
        case .success(let executable):
            do {
                let output = try TcrTool.run(executable: executable, arguments: arguments)
                guard output.exitCode == 0 else {
                    let text = output.stderr.trimmingCharacters(in: .whitespacesAndNewlines)
                    return .failed(
                        "tcr \(arguments.joined(separator: " ")) failed (exit "
                            + "\(output.exitCode)): \(text.isEmpty ? "no output" : text)")
                }
                guard let decoded = String(data: output.stdout, encoding: .utf8) else {
                    return .failed(
                        "tcr \(arguments.joined(separator: " ")) printed bytes that are not "
                            + "UTF-8.")
                }
                let printed = decoded.trimmingCharacters(in: .whitespacesAndNewlines)
                guard !printed.isEmpty else {
                    return .failed(
                        "tcr \(arguments.joined(separator: " ")) exited 0 and printed nothing.")
                }
                return .text(printed)
            } catch {
                return .failed(error.localizedDescription)
            }
        }
    }

    /// Blocking, always called off the main actor.
    ///
    /// `stdin` carries a secret when it is non-nil, so nothing below ever puts
    /// it in a message: every string here is built from `arguments`.
    /// Not `private`: ``join(link:)`` above is the app's URL handler and runs
    /// through this same one path, so a link and a pasted key cannot be
    /// invoked two different ways.
    nonisolated static func perform(arguments: [String], stdin: String?) -> Outcome {
        switch TcrTool.resolve() {
        case .failure(let notFound):
            return .failed("tcr not found (searched \(notFound.searched.count) locations)")
        case .success(let executable):
            do {
                let output = try TcrTool.run(
                    executable: executable, arguments: arguments, stdin: stdin)
                guard output.exitCode == 0 else {
                    let text = output.stderr.trimmingCharacters(in: .whitespacesAndNewlines)
                    return .failed(
                        "tcr \(arguments.joined(separator: " ")) failed (exit "
                            + "\(output.exitCode)): \(text.isEmpty ? "no output" : text)")
                }
                return .clean
            } catch {
                return .failed(error.localizedDescription)
            }
        }
    }

    /// The live half of one read: `tcr peer status --json`, classified.
    ///
    /// Never throws and never fails the tab. Three outcomes collapse to
    /// ``PeerListDocument/LivePeersRead/unsupported``, a `tcr` with no such
    /// verb (`clap` exits 2), a proxy that is not running, and an answer this
    /// build cannot parse, because all three mean the same thing to the
    /// reader: nobody measured anything, so the row keeps what the peers file
    /// said and draws no path lines. A measurement that is missing must cost
    /// the tab nothing; that is what makes this a second call rather than a
    /// wider first one.
    private nonisolated static func readLive(executable: URL) -> PeerListDocument.LivePeersRead {
        guard let output = try? TcrTool.run(
            executable: executable, arguments: PeerCommand.liveStatus)
        else { return .unsupported }
        guard output.exitCode == 0 else { return .unsupported }
        return (try? PeerListDocument.decodeLivePeers(output.stdout)) ?? .unsupported
    }

    /// Blocking, always called off the main actor.
    ///
    /// A non-zero exit from a `tcr` that has no `peer` subcommand at all is
    /// the UNSUPPORTED case, not a failure: `clap` exits 2 on an unknown
    /// subcommand, and telling an operator their proxy is broken because
    /// their CLI is old is the wrong sentence. Classified on the exit code
    /// and on `tcr`'s own "unrecognized subcommand" wording rather than on
    /// one of them alone.
    private nonisolated static func read() -> PeersSnapshot {
        switch TcrTool.resolve() {
        case .failure(let notFound):
            return PeersSnapshotBuilder.failed(
                "tcr not found (searched \(notFound.searched.count) locations). "
                    + TcrTool.overrideRemedy)
        case .success(let executable):
            do {
                let output = try TcrTool.run(
                    executable: executable, arguments: PeerCommand.list)
                if output.exitCode != 0 {
                    let text = output.stderr.lowercased()
                    if text.contains("unrecognized subcommand")
                        || text.contains("unexpected argument")
                    {
                        return PeersSnapshotBuilder.snapshot(
                            from: PeerListDocument(supported: false), now: Date())
                    }
                    let detail = output.stderr.trimmingCharacters(in: .whitespacesAndNewlines)
                    return PeersSnapshotBuilder.failed(
                        "tcr peer ls failed (exit \(output.exitCode)): "
                            + (detail.isEmpty ? "no output" : detail))
                }
                let document = try JSONDecoder().decode(
                    PeerListDocument.self, from: output.stdout)
                // The live half is a SECOND call, and it may only ever add:
                // every failure below lands as `.unsupported` and the tab
                // draws exactly the file half it always did.
                return PeersSnapshotBuilder.snapshot(
                    from: document.mergingLive(readLive(executable: executable)), now: Date())
            } catch {
                return PeersSnapshotBuilder.failed(
                    "tcr peer ls answered something this build cannot read: "
                        + error.localizedDescription)
            }
        }
    }
}

// MARK: - A Block that has been chosen and not yet confirmed

/// What the Block confirm is about: the address the question names, and the
/// argv the answer runs.
///
/// Both halves, built where the row was chosen, because they are not the same
/// string and neither is derivable from the other. A found row is banned by
/// ADDRESS; a knock is banned by its INSTANCE ID, the argument every verb on
/// that card takes, because the name in a knock is only proposed and two
/// knocks can propose one. The question still names the address either way:
/// that is the part an operator can check.
///
/// One type rather than two confirms, so the sentence an operator reads
/// before a ban cannot be written twice and drift.
struct PeerBlockTarget: Identifiable, Equatable {
    let address: String
    let arguments: [String]

    var id: String { arguments.joined(separator: " ") }
}

// MARK: - The tab

/// The Peers tab: two switches, the Macs between them, and one count line.
///
/// This is a fourth tab beside Accounts, Sessions and Tools, with nothing
/// peer-related added to the Accounts tab, and everything past the two
/// switches and Trust one level down in Settings > Peers. Built from
/// the Peers tab mockup (kept outside the tree) scenes 45 to 51 and held to its nine
/// rules; the ones this file has to keep
/// on purpose are:
///
///  - **A pill is a readout, a button is a control.** Every ``V4Pill`` here is
///    unclickable; every control is a button or a switch and none is
///    pill-shaped.
///  - **A switch's argv is the state it moves to** (``PeerCommand``).
///  - **Disclosure is only ever a sentence.** No eye glyph anywhere, and the
///    word `read` never appears alone, ``PeersSnapshotBuilder`` owns the
///    wording.
///  - **Every card with a control closes with what yes does**, in one line,
///    inside the card.
struct PeersTabV4: View {
    @ObservedObject var controller: PeerController
    /// Draw a still glyph in place of anything animated, and never start a
    /// poll. `--render-states` only, the same switch `FleetView` and
    /// ``LoginSheet`` already thread for the same `ImageRenderer` reason.
    var snapshotMode: Bool = false
    var onOpenSettings: () -> Void = {}

    /// The Trust sheet's peer, when one is open. Held here rather than on the
    /// row so two rows cannot open two sheets.
    @State private var trusting: PeerRowModel?
    /// The `tcr peer pair` behind that sheet, alive for exactly as long as it
    /// is up. One value beside ``trusting`` for the same reason: two rows
    /// cannot start two pairings.
    @State private var pairing: PeerPairRun?
    /// The Block that was chosen, while the confirm is up. One value, so two
    /// rows cannot arm two bans.
    @State private var blocking: PeerBlockTarget?

    private var snapshot: PeersSnapshot { controller.snapshot }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            // A refused verb, ABOVE the tab rather than instead of it. The
            // tab keeps every control it had, and this stays until it is
            // dismissed or a verb succeeds: the poll behind it cannot touch
            // it, because it is not part of the read (``PeerRefusal``).
            if let refused = controller.refusal.message {
                refusalBanner(refused)
            }
            if let failure = snapshot.failure {
                // The READ failed, which is a different fact: this build has
                // no peers document at all, so there is nothing to draw the
                // banner over. A refused verb no longer lands here.
                collapsed(failure)
            } else if snapshot.unsupported {
                // This must NEVER be `Unreadable status output`
                // (`FleetView.swift:1008`). That banner is for a status read
                // this build could not decode; an older `tcr` with no peer
                // subcommand decoded perfectly and answered honestly.
                collapsed(
                    "This tcr does not support peers yet. Update it and the tab fills in.")
            } else {
                if let answering = snapshot.answeringOn {
                    egressLine(answering)
                }
                findCard
                // Above "Other Macs": the shape of the mesh first, then the
                // rows that carry the same two numbers per Mac.
                //
                // NOT on the empty state. With nothing trusted this card was a
                // paragraph saying there is nothing to draw, stacked directly
                // above another card saying there is nothing found: two
                // absences, one under the other, about two fifths of the
                // panel. The found card below is the one that owns that
                // sentence.
                if snapshot.trustedCount > 0 {
                    MiniMeshCard(
                        root: snapshot.thisMac ?? "This Mac",
                        peers: snapshot.meshPeers,
                        onOpenGraph: { openGraph() }
                    )
                    .padding(.top, V4.cardGap)
                }
                // Decision row 10's own order: a request to connect sits ABOVE
                // the list of Macs, because it is the one thing on this tab
                // that is waiting on the operator. A found row is passive.
                ForEach(snapshot.pending) { knock in
                    knockCard(knock)
                }
                if snapshot.rows.isEmpty {
                    emptyCard
                } else {
                    sectionHead
                    // The rows, and ONLY the rows, inside the capped viewport:
                    // the Find card above and the Share card and count line
                    // below are pinned chrome, for the reason
                    // ``PeerPanelHeight/listHeight(rows:metrics:)`` states:
                    // a Find switch that scrolled out of reach under forty
                    // peers is the same bug as a Quit button that did.
                    if snapshotMode {
                        // `--render-states` draws the whole list unclipped:
                        // a scroll view in an `ImageRenderer` writes a PNG of
                        // its viewport, which would picture a capped list as
                        // if the rows below it did not exist.
                        peerRows
                    } else {
                        ScrollView { peerRows }
                            .frame(height: peerListHeight)
                    }
                }
                shareCard
                countLine
                if let footer = snapshot.limitedFooter {
                    limitedLine(footer)
                }
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .onAppear { if !snapshotMode { controller.start() } }
        .onDisappear { controller.stop() }
        .sheet(item: $trusting) { row in
            // The run is what the sheet draws, so there is nothing to draw
            // without one: `startPairing` already put its own refusal on the
            // tab, and this closes rather than presenting an empty sheet over
            // it.
            if let pairing {
                PeerTrustSheetHost(
                    peerName: row.title,
                    run: pairing,
                    snapshotMode: snapshotMode,
                    onFinished: { pinned in
                        if pinned { Task { await controller.refresh() } }
                    },
                    onClose: { closePairing(row: row) })
            } else {
                Color.clear.onAppear { trusting = nil }
            }
        }
        // Block asks once, and the question names the address rather than the
        // name: the name is a string the other Mac chose, and the ban is on
        // the address (and its key, where one was learned).
        .confirmationDialog(
            "Block this Mac?",
            isPresented: blockingIsPresented,
            titleVisibility: .visible,
            presenting: blocking
        ) { target in
            Button("Block \(target.address)", role: .destructive) {
                controller.run(target.arguments)
            }
            Button("Cancel", role: .cancel) {}
        } message: { target in
            Text(
                "Nothing from \(target.address) is answered again: its address, and "
                    + "its key too once this Mac has learned one. A block does not lift by "
                    + "itself. Settings > Peers > Advanced is where it is lifted.")
        }
    }

    /// Close the Trust sheet, and make sure nothing it started outlives it.
    ///
    /// The `stop()` is not tidiness. A `tcr peer pair` left running holds a
    /// handshake open for ten minutes with no surface anywhere showing it, so
    /// a sheet dismissed by any path has to take its process with it.
    ///
    /// The row stops waiting only when nothing was pinned: after a successful
    /// pairing the re-read turns it into a trusted row, and clearing the
    /// waiting flag as well would be two writers for one row.
    private func closePairing(row: PeerRowModel) {
        if let pairing {
            if case .done = pairing.state {} else { controller.stopWaiting(address: row.id) }
            pairing.stop()
        }
        pairing = nil
        trusting = nil
    }

    /// `confirmationDialog(presenting:)` needs a `Bool` binding beside the
    /// value, and clearing the value on dismiss is what keeps a cancelled
    /// dialog from leaving the last row armed for the next press.
    private var blockingIsPresented: Binding<Bool> {
        Binding(get: { blocking != nil }, set: { if !$0 { blocking = nil } })
    }

    // MARK: The peer list and its height

    /// Every peer card, in the order `tcr peer ls --json` answered in.
    private var peerRows: some View {
        VStack(alignment: .leading, spacing: 0) {
            ForEach(snapshot.rows) { row in
                peerCard(row)
                    .padding(.top, V4.cardGap)
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }

    /// How tall to draw the peer list, from ``PeerPanelHeight``, the whole
    /// reason that type is in `TcrBarCore` rather than private to this file.
    ///
    /// Derived from the SNAPSHOT (each row's shape) and never from a
    /// `GeometryReader` on what was drawn, which is the rule the panel's tab
    /// switch documents in as many words (`FleetView.swift:1070-1077`): a tab
    /// whose height depends on content it renders is one half of the
    /// layout-cycle abort `ffe8a86` fixed. A row shape is data, so this number
    /// is settled before SwiftUI lays anything out, and `PeerSectionHeightTests`
    /// runs the arithmetic without a view at all.
    private var peerListHeight: CGFloat {
        PeerPanelHeight.listViewportHeight(
            rows: snapshot.rowShapes, metrics: Self.heightMetrics)
    }

    // MARK: Collapsed

    /// One honest line, and nothing else on the tab. The collapse
    /// `FleetView.swift:1008` does for an undecodable status, in the shape a
    /// missing subcommand deserves.
    private func collapsed(_ sentence: String) -> some View {
        V4Card {
            V4Row {
                VStack(alignment: .leading, spacing: 2) {
                    NameText(text: "Peers")
                    Text(sentence)
                        .font(V4.font(V4.muteSize))
                        .foregroundStyle(Tok.mute)
                        .fixedSize(horizontal: false, vertical: true)
                        .lineSpacing(V4.lineSpacing(V4.muteSize))
                }
            }
        }
        .padding(.top, V4.marginAfterStrip(V4.cardGap))
    }

    /// The refused verb, in `tcr`'s own words, with the one control that
    /// answers it.
    ///
    /// Amber and not red: nothing is broken, an act was declined, and the
    /// `w12-exits-*` rows are the surface this copies. Colour is the second
    /// channel as everywhere here, so the sentence leads and the glyph
    /// follows it.
    private func refusalBanner(_ message: String) -> some View {
        V4Card {
            V4Row {
                HStack(alignment: .top, spacing: V4.rowGap) {
                    Image(systemName: "exclamationmark.triangle")
                        .font(V4.font(V4.dimSize))
                        .foregroundStyle(Tok.near)
                    VStack(alignment: .leading, spacing: 2) {
                        NameText(text: "That was refused", lineLimit: 2)
                        Text(message)
                            .font(V4.font(V4.muteSize))
                            .foregroundStyle(Tok.ink)
                            .fixedSize(horizontal: false, vertical: true)
                            .lineSpacing(V4.lineSpacing(V4.muteSize))
                            .frame(maxWidth: .infinity, alignment: .leading)
                            .textSelection(.enabled)
                    }
                }
            } trailing: {
                PeerActionButton(
                    title: "Dismiss",
                    systemImage: nil,
                    help: "Clears this. It also clears by itself the next time a press does "
                        + "what it was asked.",
                    enabled: true
                ) { controller.dismissRefusal() }
            }
        }
        .padding(.top, V4.marginAfterStrip(V4.cardGap))
        .accessibilityElement(children: .combine)
        .accessibilityLabel("Refused. \(message)")
    }

    // MARK: The egress line

    /// "Answering on studio-mac right now", the first thing an operator reads
    /// when their own accounts are dry, because it explains why work is still
    /// moving. It names the Mac, the direction and the count.
    private func egressLine(_ answering: PeerListDocument.AnsweringOn) -> some View {
        HStack(spacing: V4.rowGap) {
            HStack(spacing: 0) {
                Text("Answering on ")
                    .font(V4.font(V4.dimSize))
                    .foregroundColor(Tok.dim)
                Text(answering.peer)
                    .font(V4.font(V4.dimSize, .semibold))
                    .foregroundColor(Tok.ink)
                Text(" right now")
                    .font(V4.font(V4.dimSize))
                    .foregroundColor(Tok.dim)
            }
            .lineLimit(1)
            Spacer(minLength: 0)
            PeerPill(
                text: "\(answering.inFlight) in flight",
                role: .disclosure,
                help: "Requests of yours that \(answering.peer) is serving on its own accounts, "
                    + "and reading, at this moment.")
        }
        .padding(.top, V4.marginAfterStrip(V4.cardGap))
        .padding(.horizontal, V4.sectionHeadMarginSide)
    }

    // MARK: The two switches

    private var findCard: some View {
        let on = snapshot.finding
        let argv = PeerCommand.find(on: !on)
        return switchCard(
            title: "Find Macs on this network",
            // The subtitle DESCRIBES, in every state. It used to carry
            // `Looking. 2 Macs found, 2 trusted.` while the footer a few
            // points below carried the same count: one fact, printed twice in
            // one scroll, and the cost was the only line on this tab that
            // could say what finding actually does. The count now lives in the
            // footer alone.
            state: on
                ? "Looking. Other Macs running tcr appear below by themselves."
                : "Off. This Mac is not announcing itself and is not looking.",
            isOn: on,
            enabled: true,
            argv: argv,
            yes: on
                ? "This Mac announces that a tcr is here, and its name if you allow it in "
                    + "Settings. Never its id or its keys. A Mac that appears can do nothing "
                    + "until you press Trust on both screens."
                : "On announces only that a tcr is here, and this Mac's name if you allow it "
                    + "in Settings. Never its id or its keys. A Mac that appears can do "
                    + "nothing at all until you press Trust on both screens.",
            yesRole: .plain
        )
        // The strip's own margin collapses into this one only when this card
        // is the FIRST thing under the tabs. A refusal banner or the egress
        // line above it makes it an ordinary gap.
        .padding(
            .top,
            snapshot.answeringOn == nil && !controller.refusal.isShowing
                ? V4.marginAfterStrip(V4.cardGap) : V4.cardGap)
    }

    private var shareCard: some View {
        let on = snapshot.sharing
        let names = snapshot.rows.filter { $0.trust == .trusted }.map(\.title)
        return switchCard(
            title: "Share accounts with trusted Macs",
            state: shareStateLine(on: on, names: names),
            isOn: on,
            enabled: snapshot.canShare,
            argv: PeerCommand.share(on: !on),
            yes: shareYesLine(on: on, names: names),
            yesRole: on ? .disclosure : .warn
        )
        .padding(.top, V4.cardGap)
    }

    private func shareStateLine(on: Bool, names: [String]) -> String {
        guard snapshot.canShare else { return "Off, and it stays off until a Mac is trusted." }
        guard on else { return "Off. Trusted Macs carry your traffic but never read it." }
        if names.isEmpty { return "On, and applying to nobody at the moment." }
        return "On. \(PeerFormat.list(names)) may serve your requests."
    }

    private func shareYesLine(on: Bool, names: [String]) -> String {
        guard snapshot.canShare else {
            return "On means a trusted Mac may serve your requests with its own accounts, and "
                + "read them. There is nothing to share with yet."
        }
        guard on else {
            let who = names.first.map { "\($0) may" } ?? "a trusted Mac may"
            return "On means \(who) serve your requests with its accounts, and read them. "
                + "Which Mac, how much and for how long: Settings."
        }
        return "Off stops both directions at the next request: a Mac that was answering for "
            + "you stops, and you stop answering for it. Work in flight finishes."
    }

    /// One switch card: the name, the state it is in, the switch, and what yes
    /// does. The sentence is INSIDE the card by construction, there is no
    /// parameter that would let a caller put it anywhere else.
    private func switchCard(
        title: String,
        state: String,
        isOn: Bool,
        enabled: Bool,
        argv: [String],
        yes: String,
        yesRole: YesRole
    ) -> some View {
        V4Card {
            V4Row {
                VStack(alignment: .leading, spacing: 2) {
                    NameText(text: title)
                    MuteText(text: state, lineLimit: nil)
                }
            } trailing: {
                PeerSwitch(
                    isOn: isOn,
                    enabled: enabled && !controller.isPending(argv),
                    label: title,
                    help: yes
                ) { controller.run(argv) }
            }
            yesBlock(yes, role: yesRole)
        }
    }

    // MARK: A peer row

    private func peerCard(_ row: PeerRowModel) -> some View {
        V4Card {
            V4Row {
                if row.titleIsAddress {
                    MonoText(text: row.title)
                } else {
                    NameText(text: row.title)
                }
            } trailing: {
                // ONE trailing column, explicitly stacked. `V4Row` hands its
                // trailing closure a `fixedSize()` and a layout priority, so
                // two loose views there would be two children fighting for
                // one width budget and every point they took would come out
                // of the peer's NAME (this row's own measurement, at
                // `freshnessDot`).
                HStack(spacing: V4.pillGap) {
                    switch row.trust {
                    case .found(let dialAddress, _):
                        PeerActionButton(
                            title: "Trust",
                            // No glyph. A checkmark is the universal "already
                            // done", and this row's own sub-line two lines
                            // below reads `not trusted`. The word is the
                            // control.
                            systemImage: nil,
                            // What the press really does, per decision row 10:
                            // it SENDS a request. The six digits are phase 2
                            // and cannot appear until somebody on that Mac
                            // accepts, so this no longer promises them.
                            help: "Asks \(row.title) to connect. Six digits appear on both "
                                + "screens once somebody there accepts, and nothing changes "
                                + "before that.",
                            // One pairing at a time: the sheet IS the pairing,
                            // and a second press behind an open sheet would
                            // start a second held-open handshake this panel
                            // has nowhere to draw.
                            enabled: trusting == nil
                        ) {
                            guard
                                let run = controller.startPairing(
                                    rowId: row.id, dialAddress: dialAddress)
                            else { return }
                            pairing = run
                            trusting = row
                        }
                    case .waiting(let address):
                        PeerActionButton(
                            title: "Cancel",
                            systemImage: nil,
                            help: PeerAdmission.cancelWaitingHelp,
                            enabled: true
                        ) { controller.stopWaiting(address: address) }
                    case .trusted:
                        ForEach(row.pills, id: \.text) { pill in
                            PeerPill(text: pill.text, role: pill.role, help: pillHelp(pill.text))
                        }
                        freshnessDot(row)
                    }
                    rowMenu(row)
                }
            }
            switch row.trust {
            case .found(_, let promise):
                MuteText(text: row.freshness, lineLimit: nil)
                yesBlock(promise, role: .plain)
            case .waiting:
                MuteText(text: row.freshness, lineLimit: nil)
                yesBlock(PeerAdmission.waitingSentence, role: .plain)
            case .trusted:
                meterView(row.meter)
                // The locator half of the row, under the meter: every way this
                // Mac knows to reach that one. Absent entirely on a `tcr`
                // whose live half this build could not read, which is why the
                // row above it is unchanged rather than reworded.
                ForEach(row.pathLines, id: \.self) { line in
                    Text(line.text)
                        .font(V4.font(V4.muteSize))
                        .foregroundStyle(pathTint(line.tone))
                        .lineLimit(1)
                        .frame(maxWidth: .infinity, alignment: .leading)
                }
            }
        }
        // No blanket opacity on an ended row. `V4.endedGlyphOpacity`'s own
        // note carries the measurements: dimming the card put every text run
        // on it, and its only control, below AA. The `lease ended` pill, the
        // `ended` label and the sentence carry the meaning at full ink, and
        // the one element with no word on it is what dims (`freshnessDot`).
    }

    /// Serve the mesh as a page and open it. A refusal is surfaced through
    /// the same failure the rest of the tab uses, never swallowed: a link that
    /// did nothing and said nothing is the one outcome this tab forbids.
    private func openGraph() {
        guard !snapshotMode else { return }
        if let refusal = PeerGraphLauncher.open() {
            controller.report(failure: refusal)
        }
    }

    /// Ordinary, worth a look, or the absence itself. Colour is the second
    /// channel here as everywhere: `no path right now` says so in words, and
    /// a forwarded line names the forwarder.
    private func pathTint(_ tone: PeerPathLine.Tone) -> Color {
        switch tone {
        case .plain: return Tok.mute
        case .warn: return Tok.near
        case .absent: return Tok.disabled
        }
    }

    /// A found row's own menu, and the only thing in it is Block.
    ///
    /// # What it closes
    ///
    /// Decision row 11 shipped the ban and it had exactly two call sites, both
    /// a knock row, so an address could be banned only WHILE it was knocking:
    /// a Mac flooding the found list, or one whose knock expired ten minutes
    /// ago, could not be banned at any click depth. The found list is where
    /// that Mac is, so that is where the control goes.
    ///
    /// # Why a menu, and why not on a trusted row
    ///
    /// A menu rather than a fourth button, for the reason the knock card's own
    /// review finding gives: an irreversible act must not be a same-weight
    /// target beside the ordinary ones. It is behind one press,
    /// destructive-roled, and it asks before it writes.
    ///
    /// A TRUSTED row does not get one, and that is measured rather than
    /// chosen. Its trailing column already carries two pills, the freshness
    /// readout and the refresh dot, and `V4Row` clips the LEADING label to pay
    /// for anything added there: the first render of this control drew
    /// `studio-…` where the peer's name goes
    /// (`renders/wave8c-item5/states/50-peers-borrowing-dark.png`), which is
    /// the same defect `pills(_:awake:)` documents at two pills. A trusted
    /// Mac's acts live in its own sheet, the pane's header says so in as
    /// many words, so Block joins Forget there, beside the room to explain
    /// it.
    ///
    /// Nothing is drawn for a row with no address: the verb takes one, and a
    /// menu offering an act it cannot perform is worse than no menu.
    @ViewBuilder
    private func rowMenu(_ row: PeerRowModel) -> some View {
        if row.trust != .trusted, let address = row.address {
            blockMenu(
                address: address,
                arguments: PeerCommand.block(address: address),
                accessibilityLabel: "More for \(row.title)",
                help: "Block this address, whether or not it is asking to connect.")
        }
    }

    /// One menu, for the two rows that may ban: a found row and a knock.
    ///
    /// Its single item is destructive and it asks before it writes. Shared
    /// rather than written twice, because the two rows differ in exactly one
    /// thing, the argv, and a second spelling of a ban's own control is how
    /// one of them ends up without a confirm in front of it.
    @ViewBuilder
    private func blockMenu(
        address: String, arguments: [String], accessibilityLabel: String, help: String
    ) -> some View {
        if snapshotMode {
            // `ImageRenderer` rasterises a `Menu` as the macOS "prohibited"
            // placeholder, measured again here, as `accountActionsMenu`
            // records for the Accounts tab: the first render of this row drew
            // a yellow circle-slash where the glyph goes. The still label is
            // what the live panel shows, so the PNG pictures the control
            // rather than the harness's own limit.
            rowMenuLabel
        } else {
            Menu {
                Button("Block \(address)…", role: .destructive) {
                    blocking = PeerBlockTarget(address: address, arguments: arguments)
                }
            } label: {
                rowMenuLabel
            }
            .menuStyle(.borderlessButton)
            .menuIndicator(.hidden)
            .fixedSize()
            .accessibilityLabel(accessibilityLabel)
            .help(help)
        }
    }

    /// The glyph the live `Menu` and its render stand-in share, so the two
    /// cannot draw two different icons.
    private var rowMenuLabel: some View {
        Image(systemName: "ellipsis")
            .font(.system(size: 11, weight: .semibold))
            .foregroundStyle(Tok.mute)
            .frame(width: V4.rowMenuIconWidth, height: PeersTabV4.hitTarget)
            .contentShape(Rectangle())
    }

    /// The freshness readout AND the control that re-reads it, which is why it
    /// is a button and the pills beside it are not (the mockup's rule 3, and
    /// its own scene 48 note).
    private func freshnessDot(_ row: PeerRowModel) -> some View {
        HStack(spacing: 5) {
            Text(row.freshness)
                .font(V4.font(V4.muteSize))
                .foregroundStyle(Tok.mute)
                .lineLimit(1)
            Button {
                Task { await controller.refresh() }
            } label: {
                Circle()
                    .fill(row.awake ? Tok.ok : Tok.inkFaint)
                    // The one element on an ended row that dims. It carries
                    // no word, so nothing legible is spent on saying "past".
                    .opacity(row.meter.isEnded ? V4.endedGlyphOpacity : 1)
                    .frame(width: V4.dotSize, height: V4.dotSize)
                    // A 40 pt target that costs 8 pt of layout.
                    //
                    // Measured, not reasoned: charging the row the full 40 pt
                    // for an 8 pt dot left `studio-mac` rendering as
                    // `studio-…` beside two pills, `V4Row` clips the leading
                    // label, so every point the trailing column takes comes
                    // out of the peer's NAME. The negative padding gives the
                    // layout the dot's own size back while the content shape
                    // stays the 40 pt touch-target floor, which is the
                    // standard way to keep a small affordance
                    // pressable without redrawing the row around it.
                    .frame(width: PeersTabV4.hitTarget, height: PeersTabV4.hitTarget)
                    .contentShape(Rectangle())
                    .padding(-(PeersTabV4.hitTarget - V4.dotSize) / 2)
            }
            .buttonStyle(.plain)
            .accessibilityLabel("Check \(row.title) now")
            .help("Read tcr peer ls again now.")
        }
    }

    @ViewBuilder
    private func meterView(_ meter: PeerMeter) -> some View {
        switch meter {
        case .none(let sentence):
            if let sentence {
                MuteText(text: sentence, lineLimit: nil)
            }
        case .lease(let fraction):
            LeaseMeter(fraction: fraction)
        case .gateway(let bytes):
            GatewayMeter(bytes: bytes)
        case .ended(let ended):
            endedBlock(ended)
        }
    }

    /// An ended lease: what it was, when it stopped, and the one control that
    /// starts it again.
    ///
    /// Greyed by ``V4/endedRowOpacity`` on the whole card at the call site, so
    /// the row reads as past at a glance and the words carry the meaning.
    private func endedBlock(_ ended: LeaseEnded) -> some View {
        VStack(alignment: .leading, spacing: 3) {
            HStack(spacing: V4.rowGap) {
                Text(ended.label)
                    .font(V4.font(V4.muteSize))
                    .foregroundStyle(Tok.mute)
                Spacer(minLength: 0)
                if let arguments = ended.relendArguments {
                    PeerActionButton(
                        title: "Re-lend",
                        systemImage: nil,
                        // Not a destructive control, and not a warning: it
                        // grants again, which is the mockup's own reading of
                        // the greyed row's `mini` button.
                        help: "Lends the same scope and the same amount again, starting now.",
                        enabled: !controller.isPending(arguments)
                    ) { controller.run(arguments) }
                }
            }
            MuteText(text: ended.sentence, lineLimit: nil)
        }
    }

    // MARK: A Mac asking to connect

    /// `<name> (<addr>) wants to connect`, with Accept and Ignore.
    ///
    /// Decision row 10, and every word of it is load-bearing. **Nothing has
    /// happened yet**: a knock reveals no static key, so there is nothing
    /// pinned, nothing carried, nothing served, and no six digits to compare:
    /// Accept is what opens the 120-second window in which those appear. The
    /// sentence under the title says so, because a row with an Accept button
    /// on it reads like a row where accepting IS the trust.
    ///
    /// Three controls and not two. The mockup's scene 59 draws Ignore and
    /// Accept; Block is here too because row 11 shipped the ban and the
    /// Blocked list one pane down can only be reached through Settings, which
    /// is a long way to go with somebody knocking at you. Ignore is quiet for
    /// an hour and Block is forever, so Block carries the destructive role and
    /// Ignore does not.
    ///
    /// Every verb is handed the INSTANCE ID. The proposed name is a string a
    /// stranger on this network chose, and two knocks can propose the same
    /// one.
    private func knockCard(_ knock: PeerKnock) -> some View {
        // Each verb's argv is built ONCE and used for both the press and the
        // enabled check. It was written twice per control, and a mutation run
        // caught what that costs: changing what Accept RUNS left the
        // in-flight check watching the command it used to run, so a button
        // could be enabled on one argv and press another. Two spellings of
        // one decision, and the one that drifts is the one nobody reads.
        // Block is NOT here. It is forever, and it sat as the rightmost equal
        // of two reversible answers: Ignore is quiet for an hour, Accept opens
        // a two-minute window, and the third button next to them never lifts
        // by itself. It is in the row's own menu now, destructive and behind a
        // confirm, the shape the found row already uses. What is left is the
        // two ordinary answers to "wants to connect".
        let verbs: [(title: String, argv: [String], help: String, destructive: Bool)] = [
            (
                "Ignore", PeerCommand.ignore(instance: knock.instanceId),
                "Turns this request down and stays quiet to that address for an hour. A "
                    + "request nobody answers expires by itself; the line above counts "
                    + "what is left of it.",
                false
            ),
            (
                "Accept", PeerCommand.accept(instance: knock.instanceId),
                "Opens a two-minute window for this one Mac. Both screens then show six "
                    + "digits and nothing is shared until you press Trust on both.",
                false
            ),
        ]
        return V4Card {
            VStack(alignment: .leading, spacing: V4.knockLineGap) {
                // The whole card width for the sentence, and the buttons on
                // their own row underneath.
                //
                // They used to sit in a `V4Row`'s trailing slot, which gives
                // the trailing column `layoutPriority(1)` and clips the
                // leading label to pay for it: three buttons squeezed the
                // sentence to about 130 pt and it rendered `loft-mini wants
                // t…`. The one line on this tab that says a stranger's Mac is
                // asking to connect was cut mid-word, verb gone, in both
                // appearances. Nothing about these three controls needs to be
                // on the title's line.
                //
                // The proposed name (or the address, when no name was sent) on
                // its own line, and the address ALWAYS on a second line rather
                // than folded into one via `knockTitle`: "loft-mini
                // (10.0.1.24) wants to connect" truncated to "loft-mini
                // (10.0.1…" mid address, which is exactly the part an operator
                // is meant to be able to check.
                HStack(alignment: .top, spacing: V4.pillGap) {
                    NameText(text: PeerAdmission.knockNameLine(knock), lineLimit: 2)
                    Spacer(minLength: 0)
                    // The ban, one press away from the row it is about and
                    // never a button beside the two reversible answers.
                    blockMenu(
                        address: knock.addr,
                        arguments: PeerCommand.block(instance: knock.instanceId),
                        accessibilityLabel: "More for \(PeerAdmission.knockNameLine(knock))",
                        help: "Block this address, whether or not it is asking to connect.")
                }
                // The address AND the deadline, counted from the knock's own
                // first-seen time against the clock this snapshot was read
                // at, rather than a card stating ten minutes in prose and
                // never counting them.
                if let address = PeerAdmission.knockAddressLine(knock, now: snapshot.readAt) {
                    MuteText(text: address, lineLimit: 1)
                }
                MuteText(text: PeerAdmission.knockDetail, lineLimit: nil)
                HStack(spacing: V4.pillGap) {
                    Spacer(minLength: 0)
                    ForEach(verbs, id: \.title) { verb in
                        PeerActionButton(
                            title: verb.title,
                            systemImage: nil,
                            help: verb.help,
                            enabled: !controller.isPending(verb.argv),
                            action: { controller.run(verb.argv) })
                    }
                }
                yesBlock(
                    "Accept shows six digits here and on that Mac. Until you press Trust on "
                        + "both it is pinned nowhere, carries nothing and serves nothing.",
                    role: .warn)
            }
        }
        .padding(.top, V4.cardGap)
    }

    /// `12 shown, 3 more not shown`, under the count line.
    ///
    /// Decision row 11 caps found rows at 12, and 2 per address. A list that
    /// silently stopped at the cap looks exactly like a network with twelve
    /// Macs on it, which is the one reading an operator must not be left with.
    private func limitedLine(_ footer: String) -> some View {
        Text(footer)
            .font(V4.font(V4.byToolLineSize))
            .foregroundStyle(Tok.mute)
            .padding(.top, V4.limitedLineTopMargin)
            .padding(.horizontal, V4.sectionHeadMarginSide)
            .help(
                "This Mac shows at most 12 found Macs, and at most 2 from one address, so a "
                    + "noisy network cannot fill the tab. The rest are still there.")
    }

    // MARK: Empty, head and count

    /// The empty state says what the switch WILL do rather than reporting an
    /// absence, so the tab teaches the feature before it has any data.
    /// "Looking" and "no peers" are two different fields, so this never has to
    /// guess which of the two it is in.
    private var emptyCard: some View {
        V4Card {
            VStack(alignment: .leading, spacing: 3) {
                NameText(text: snapshot.finding ? "Nothing found yet" : "No other Macs")
                MuteText(
                    text: snapshot.finding
                        ? "Only Macs on this network, running tcr, with finding on, can "
                            + "appear. A Mac elsewhere is added by hand in Settings."
                        : "Turn finding on and any Mac running tcr on this network appears "
                            + "here by itself.",
                    lineLimit: nil)
            }
        }
        .padding(.top, V4.cardGap)
    }

    private var sectionHead: some View {
        VStack(alignment: .leading, spacing: 2) {
            HStack(spacing: V4.rowGap) {
                SectionHead(title: "Other Macs")
                Spacer(minLength: 0)
                if snapshot.sharing {
                    PeerPill(
                        text: "sharing", role: .disclosure,
                        help: "Trusted Macs may serve your requests on their own accounts, and "
                            + "read them.")
                } else if snapshot.finding {
                    PeerPill(
                        text: "finding", role: .ok,
                        help: "Discovery is up. A readout, not a control.")
                }
            }
            // The fact every trusted row used to repeat, said once, where a
            // reader meets it before the rows rather than seven times inside
            // them. Drawn whenever the head is, so the height budget's
            // `carrySentenceLines` is a constant and not a guess.
            MuteText(text: PeersSnapshotBuilder.carrySentence, lineLimit: nil)
        }
        .padding(.top, V4.sectionHeadMarginTop)
        .padding(.horizontal, V4.sectionHeadMarginSide)
    }

    private var countLine: some View {
        HStack(spacing: V4.rowGap) {
            Text(snapshot.countLine)
                .font(V4.font(V4.byToolLineSize))
                .foregroundStyle(Tok.mute)
            Spacer(minLength: 0)
            PeerActionButton(
                title: "Settings…",
                systemImage: nil,
                help: "Which window, how much of it, for how long, and who may serve: "
                    + "Settings > Peers.",
                enabled: true,
                action: onOpenSettings)
        }
        .padding(.top, V4.footerMarginTop)
        .padding(.horizontal, V4.sectionHeadMarginSide)
    }

    // MARK: What yes does

    enum YesRole {
        case plain
        case warn
        case disclosure

        var tint: Color {
            switch self {
            case .plain: return Tok.cardLine
            case .warn: return Tok.near
            case .disclosure: return Tok.unknown
            }
        }
    }

    /// The mockup's `.yes` block: what pressing yes does, inside the card,
    /// with the reserved hue when what it does is disclose plaintext. Colour
    /// never carries the meaning alone, the sentence is always beside it.
    private func yesBlock(_ sentence: String, role: YesRole) -> some View {
        Text(sentence)
            .font(V4.font(V4.muteSize))
            .foregroundStyle(Tok.dim)
            .fixedSize(horizontal: false, vertical: true)
            .lineSpacing(V4.lineSpacing(V4.muteSize))
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(.vertical, V4.yesBlockPaddingV)
            .padding(.horizontal, V4.yesBlockPaddingH)
            .background(
                RoundedRectangle(cornerRadius: V4.buttonRadius)
                    .fill(role.tint.opacity(0.07))
            )
            .overlay(
                RoundedRectangle(cornerRadius: V4.buttonRadius)
                    .strokeBorder(role.tint.opacity(0.35), lineWidth: V4.panelBorderWidth)
            )
            .padding(.top, V4.yesBlockMarginTop)
    }

    // MARK: Geometry

    /// macOS' pointer minimum is 24 pt (`V4.killHitTarget`'s own note) and
    /// this tab sets 40 pt on every switch and button here, which is the
    /// touch-target floor rather than the pointer one. The VISUAL stays the
    /// mockup's size and the hit area is grown around it: a control that looks
    /// as big as its target would redraw this tab at a density the design does
    /// not have.
    static let hitTarget: CGFloat = 40

    /// How many lines ``PeersSnapshotBuilder/carrySentence`` takes under the
    /// section head at this panel's width, for the height budget below.
    ///
    /// Charged unconditionally, because the sentence is drawn whenever the
    /// section head is: a conditional line is a line the budget can be wrong
    /// about, and growth the budget cannot see comes out of the footer
    /// (``PeerPanelHeight``'s own invariant).
    static let carrySentenceLines: CGFloat = 2

    /// What ``PeerPanelHeight`` charges for this tab, at the density now
    /// resolved. Read off `V4` here (the one place that knows both), and
    /// passed in, so the arithmetic stays testable in `TcrBarCore`.
    static var heightMetrics: PeerPanelHeight.Metrics {
        PeerPanelHeight.Metrics(
            nameLineHeight: V4.lineHeight(V4.nameSize),
            subLineHeight: V4.lineHeight(V4.muteSize),
            meterHeight: V4.barHeight + V4.quotaMarginTop + V4.lineHeight(V4.muteSize),
            cardChrome: 2 * V4.cardInsetV,
            cardGap: V4.cardGap,
            // The two switch cards (each a name line, a state line and a
            // two-line yes block inside its own card chrome), the section
            // head AND the carry sentence under it, and the count line.
            fixedChrome: 2
                * (V4.lineHeight(V4.nameSize) + V4.lineHeight(V4.muteSize)
                    + 2 * V4.lineHeight(V4.muteSize) + 2 * V4.cardInsetV)
                + V4.sectionHeadMarginTop + V4.lineHeight(V4.sectionHeadSize)
                + carrySentenceLines * V4.lineHeight(V4.muteSize)
                + V4.footerMarginTop + V4.lineHeight(V4.byToolLineSize))
    }

    private func pillHelp(_ text: String) -> String {
        switch text {
        case "trusted": return "Pinned on both Macs. It can carry your traffic."
        case PeersSnapshotBuilder.carryPillText:
            return "May hold your encrypted bytes and can open none of them. The path line "
                + "under the row says whether it is doing so now."
        case PeerLendDirection.youLend.pillText:
            return "May serve your requests on its own accounts, and read them."
        case PeerLendDirection.theyLend.pillText:
            return "Serving your requests on its own accounts right now, and reading them."
        case "asleep": return "Not answering. The offer stands and resumes when it wakes."
        case "no headroom": return "It has nothing spare, so nothing was asked of it."
        default: return text
        }
    }
}

// MARK: - The two meter views

/// A lease meter, and it takes a ``LeaseFraction`` and nothing else.
///
/// Together with ``GatewayMeter`` this is typed so a gateway row cannot
/// render a fraction meter and a lease row cannot render a byte meter: the
/// wrong meter does not compile. The two payload types
/// are unrelated structs with no shared protocol and no conversion between
/// them, and ``PeerMeter``'s arms each carry exactly one, so swapping these two
/// views at their call sites is a type error rather than a wrong picture.
struct LeaseMeter: View {
    let fraction: LeaseFraction

    var body: some View {
        MeterBody(
            label: fraction.label,
            fill: fraction.spent,
            value: fraction.value,
            sentence: fraction.sentence,
            tint: Tok.unknown)
    }
}

/// A gateway byte meter, and it takes a ``GatewayBytes`` and nothing else.
/// See ``LeaseMeter`` for why the pair is typed this way.
struct GatewayMeter: View {
    let bytes: GatewayBytes

    var body: some View {
        MeterBody(
            label: "carried",
            fill: bytes.fraction,
            value: bytes.value,
            sentence: bytes.sentence,
            // Carrying is blind, so it is NOT the reserved plaintext hue. The
            // one hue on this tab that means "another Mac reads this" is
            // spent on nothing else.
            tint: V4.info)
    }
}

/// The bar both meters draw. Private geometry, no payload of its own: it takes
/// a number and a string, so it cannot be the place a unit goes missing.
private struct MeterBody: View {
    /// Wide enough for `carried`, the longer of the two labels, so the two
    /// meters' bars start at one x whichever kind of row draws them.
    static let labelWidth: CGFloat = 48

    let label: String
    let fill: Double
    let value: String
    let sentence: String
    let tint: Color

    var body: some View {
        VStack(alignment: .leading, spacing: 2) {
            HStack(spacing: V4.quotaGap) {
                Text(label)
                    .font(V4.font(V4.muteSize))
                    .foregroundStyle(Tok.mute)
                    .frame(width: MeterBody.labelWidth, alignment: .leading)
                GeometryReader { proxy in
                    ZStack(alignment: .leading) {
                        RoundedRectangle(cornerRadius: V4.barRadius)
                            .fill(Tok.ink.opacity(V4.barTrackAlpha))
                        RoundedRectangle(cornerRadius: V4.barRadius)
                            .fill(tint)
                            .frame(
                                width: max(
                                    fill <= 0 ? 0 : V4.barMinWidth,
                                    proxy.size.width * CGFloat(min(1, max(0, fill)))))
                    }
                }
                .frame(height: V4.barHeight)
                Text(value)
                    .font(V4.font(V4.muteSize).monospacedDigit())
                    .foregroundStyle(fill > 0 ? tint : Tok.mute)
                    .lineLimit(1)
                    .fixedSize()
            }
            .padding(.top, V4.quotaMarginTop)
            Text(sentence)
                .font(V4.font(V4.muteSize))
                .foregroundStyle(Tok.mute)
                .fixedSize(horizontal: false, vertical: true)
                .lineSpacing(V4.lineSpacing(V4.muteSize))
        }
        .accessibilityElement(children: .combine)
        .accessibilityLabel("\(label), \(value)")
        .accessibilityValue(sentence)
    }
}

// MARK: - The controls

/// The tab's switch, drawn, not a `Toggle`.
///
/// `ImageRenderer` rasterises a `.switch` (and a `.checkbox`) toggle as the
/// macOS "prohibited" placeholder regardless of `isOn`: `RenderStates`'s own
/// header records the measurement, and the awake row's two scenes are the
/// proof that nothing about a real toggle's ON state is reviewable in a
/// render. A tab whose entire common path IS two switches cannot be reviewed
/// through a harness that cannot draw them, so these are shapes, which also
/// lets the ON state carry the mockup's own geometry rather than the system's.
///
/// The hit area is ``PeersTabV4/hitTarget`` (40 pt) around a 34×20 visual,
/// the tab's touch-target floor, and the accessibility element is a real
/// `isToggle` switch with its own label and value.
struct PeerSwitch: View {
    let isOn: Bool
    let enabled: Bool
    let label: String
    let help: String
    let action: () -> Void

    private static let trackWidth: CGFloat = 34
    private static let trackHeight: CGFloat = 20
    private static let knob: CGFloat = 16

    var body: some View {
        Button(action: action) {
            ZStack(alignment: isOn ? .trailing : .leading) {
                Capsule()
                    .fill(isOn ? Tok.ok.opacity(0.85) : Tok.ink.opacity(V4.barTrackAlpha))
                    .overlay(
                        Capsule().strokeBorder(
                            isOn ? Tok.ok.opacity(0.5) : Tok.cardLine,
                            lineWidth: V4.panelBorderWidth))
                Circle()
                    .fill(Tok.panelTopEdge)
                    .frame(width: Self.knob, height: Self.knob)
                    .padding(V4.peerSwitchKnobInset)
            }
            .frame(width: Self.trackWidth, height: Self.trackHeight)
            .opacity(enabled ? 1 : 0.4)
            .frame(
                minWidth: PeersTabV4.hitTarget, minHeight: PeersTabV4.hitTarget,
                alignment: .trailing
            )
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .disabled(!enabled)
        .accessibilityAddTraits(.isButton)
        .accessibilityLabel(label)
        .accessibilityValue(isOn ? "on" : "off")
        .accessibilityHint(help)
        .help(help)
    }
}

/// `Trust` and `Settings…`: ``V4Button``'s look with the 40 pt hit area this
/// tab holds to. Not a change to `V4Button`, every other tab's buttons are
/// pointer targets at the panel's own density, and widening that type would
/// re-space three tabs to fix one.
struct PeerActionButton: View {
    let title: String
    var systemImage: String?
    var help: String
    var enabled: Bool = true
    let action: () -> Void

    var body: some View {
        Button(action: action) {
            HStack(spacing: 4) {
                if let systemImage {
                    Image(systemName: systemImage)
                        .font(.system(size: 11, weight: .semibold))
                }
                Text(title)
                    .font(V4.font(V4.buttonFontSize))
                    .lineLimit(1)
            }
            .foregroundStyle(enabled ? Tok.ink : Tok.inkFaint)
            .padding(.vertical, V4.buttonInsetV)
            .padding(.horizontal, V4.buttonInsetH)
            .background(
                RoundedRectangle(cornerRadius: V4.buttonRadius)
                    .fill(Tok.ink.opacity(V4.buttonFillAlpha))
            )
            .overlay(
                RoundedRectangle(cornerRadius: V4.buttonRadius)
                    .strokeBorder(Tok.cardLine, lineWidth: V4.panelBorderWidth)
            )
            .frame(minWidth: PeersTabV4.hitTarget, minHeight: PeersTabV4.hitTarget)
            .contentShape(Rectangle())
        }
        .buttonStyle(V4PressStyle(cornerRadius: V4.buttonRadius))
        .disabled(!enabled)
        .accessibilityLabel(title)
        .help(help)
    }
}

// MARK: - The Trust sheet

/// The Trust sheet plus the process behind it.
///
/// A wrapper rather than an `@ObservedObject` on ``PeerTrustSheet`` itself: the
/// sheet is then a pure function of a ``PeerPairState``, which is what lets
/// `--render-states` draw all five of its states from fixtures and what lets a
/// test build one without a subprocess. Everything that has to observe a live
/// run is here.
struct PeerTrustSheetHost: View {
    let peerName: String
    @ObservedObject var run: PeerPairRun
    var snapshotMode: Bool = false
    /// When this panel sent the request, which is the instant this host was
    /// first built: the sheet IS the pairing, and it opens on the same press
    /// that starts the process. `@State` so a redraw does not restart the
    /// count.
    @State private var sentAt = Date()
    /// Called once the pairing has settled, with whether a key was pinned. The
    /// tab re-reads on `true`: the peers file changed and the row is a trusted
    /// row now.
    var onFinished: (Bool) -> Void = { _ in }
    var onClose: () -> Void = {}

    var body: some View {
        PeerTrustSheet(
            peerName: peerName,
            state: run.state,
            compare: $run.compare,
            submitting: run.submitting,
            // What is left of the request this panel sent, counted from the
            // press that opened this sheet against the deadline the Mac
            // holding the knock enforces.
            expiresIn: PeerAdmission.knockExpirySeconds - Date().timeIntervalSince(sentAt),
            snapshotMode: snapshotMode,
            onTrust: { run.submitComparedCode() },
            // Cancel on a live run stops the child and leaves the sheet
            // saying so; on a settled one there is nothing to stop and the
            // button is Close.
            onCancel: {
                if run.state.isLive {
                    run.cancel()
                } else {
                    onClose()
                }
            }
        )
        .onChange(of: run.state) { state in
            guard !state.isLive else { return }
            if case .done = state { onFinished(true) }
        }
    }
}

/// The six-digit compare, in ``LoginSheet``'s shape.
///
/// The AirPods and Apple TV pairing shape, which is the whole reason it is six
/// digits and not a fingerprint: both screens show a number and a person reads
/// one off the other screen. The code comes from the handshake itself
/// (`get_handshake_hash()`), so comparing it binds THIS connection rather than
/// two pasted strings.
///
/// # What this sheet had to become
///
/// It took a `code: String?` and its Trust button read `enabled: code != nil`.
/// Its only call site passed a literal `nil`. So the button could never be
/// pressed, and `tcr peer pair`, which the press had already started, sat on
/// a `read_line` with no stdin for ten minutes and then exited refusing. The
/// one job this tab exists for could not be finished in the UI at all.
///
/// Now it draws a typed ``PeerPairState`` fed by a live ``PeerPairRun``, and
/// the digits go BACK down that process's stdin. Five states, each with its own
/// words: asking (the far Mac's instruction, by instance id), comparing (this
/// Mac's digits large, a field for the other Mac's), done, refused (the CLI's
/// own line) and cancelled.
///
/// **There is no "they match" button, and that is a refusal rather than an
/// omission.** A button spelled that way lets an operator confirm without ever
/// looking at the other screen, which is the one thing the six digits exist to
/// force. The field makes the comparison the only way through, and the compare
/// itself is done by the process holding the handshake.
///
/// `snapshotMode` draws a still glyph where the spinner goes, and a still plate
/// where the text field goes, for the reason ``LoginSheet`` and ``PeerSwitch``
/// record: `ImageRenderer` rasterises a `ProgressView` and an AppKit-backed
/// control as the macOS "prohibited" placeholder, so a fixture drawn through
/// them would picture the harness's limit rather than the sheet.
struct PeerTrustSheet: View {
    let peerName: String
    /// Where the pairing is, as the running command reported it. Never a
    /// literal at a call site: ``PeersTabV4`` builds it from ``PeerPairRun``.
    let state: PeerPairState
    /// The digits read off the OTHER Mac. A binding, because the field writes
    /// it and ``PeerPairRun`` is what sends it.
    @Binding var compare: PeerPairCompare
    /// Whether the digits are already on their way down the pipe.
    var submitting: Bool = false
    /// Seconds left on the request this panel sent, or `nil` when there is
    /// nothing to count. A fixture passes nothing and the sheet states no
    /// deadline, so a rendered picture of it is the same on every run.
    var expiresIn: TimeInterval?
    var snapshotMode: Bool = false
    var onTrust: () -> Void = {}
    var onCancel: () -> Void = {}

    var body: some View {
        VStack(alignment: .leading, spacing: V4.buttonGap) {
            HStack(spacing: V4.rowGap) {
                glyph
                Text(state.title(peerName: peerName))
                    .font(V4.font(V4.summarySize, .semibold))
                    .foregroundStyle(Tok.ink)
                    .fixedSize(horizontal: false, vertical: true)
            }

            // The digit block is drawn only when there ARE digits, which is
            // exactly the `comparing` state. A placeholder where a number
            // belongs reads as "digits present but masked", and the sentence
            // under it would be asserting something about the other screen
            // while the request sits unanswered over there.
            if case .comparing(let code) = state {
                codeBlock(code)
                comparedField
            }

            Text(state.sentence(peerName: peerName, expiresIn: expiresIn))
                .font(V4.font(V4.dimSize))
                .foregroundStyle(Tok.inkDim)
                .fixedSize(horizontal: false, vertical: true)
                .lineSpacing(V4.lineSpacing(V4.dimSize))
                .frame(maxWidth: .infinity, alignment: .leading)

            // The other Mac's half of the job, named while this one waits. A
            // "waiting" sheet with no instruction leaves the person at the
            // other screen with nothing to do, which is how a pairing sits for
            // ten minutes and then refuses itself.
            if let instruction = state.farSideInstruction {
                Text(instruction)
                    .font(V4.font(V4.muteSize))
                    .foregroundStyle(Tok.mute)
                    .fixedSize(horizontal: false, vertical: true)
                    .lineSpacing(V4.lineSpacing(V4.muteSize))
                    .frame(maxWidth: .infinity, alignment: .leading)
            }

            HStack(spacing: V4.buttonGap) {
                Spacer(minLength: 0)
                PeerActionButton(
                    title: state.isLive ? "Cancel" : "Close", systemImage: nil,
                    help: cancelHelp
                ) { onCancel() }
                if state.isLive {
                    PeerActionButton(
                        // No glyph here either: this button carries the same
                        // "already done" checkmark while it is DISABLED,
                        // which is the state it spends most of its life in.
                        title: "Trust", systemImage: nil,
                        help: trustHelp,
                        enabled: canTrust
                    ) { onTrust() }
                }
            }
        }
        .padding(.vertical, V4.cardPaddingV)
        .padding(.horizontal, V4.cardPaddingH)
        .frame(width: V4.panelWidth, alignment: .leading)
        .background(Tok.panel)
    }

    /// Trust is pressable only with six digits in the field and nothing
    /// already in flight. Never on `asking`, where there is nothing to
    /// compare against yet.
    private var canTrust: Bool {
        guard case .comparing = state else { return false }
        return compare.isComplete && !submitting
    }

    private var cancelHelp: String {
        switch state {
        case .asking:
            // The two commands and the instance id live HERE, not on the
            // sheet: the sheet's own line is for the person at the other Mac,
            // who has this same tab and a button; this is for the one at a
            // terminal, and for a bug report.
            let commands = state.farSideCommands.map { " " + $0 } ?? ""
            return "Stops waiting and ends this request here. The request stands on that Mac "
                + "until somebody answers it or it expires." + commands
        case .comparing:
            return "Ends this request. Nothing is written and \(peerName) stays untrusted."
        case .done, .refused, .cancelled:
            return "Closes this."
        }
    }

    private var trustHelp: String {
        switch state {
        case .asking:
            return "Waits for the digits. There is nothing to compare until somebody on "
                + "\(peerName) accepts the request."
        default:
            return "Sends the digits you typed to be compared with this handshake's own. A "
                + "match pins \(peerName)'s key; a mismatch is refused and not retried."
        }
    }

    /// This Mac's six digits, in two groups of three, so the eye compares
    /// them one group at a time.
    private func codeBlock(_ code: String) -> some View {
        HStack(spacing: V4.pairCodeGap) {
            ForEach(Array(groups(of: code).enumerated()), id: \.offset) { group in
                Text(group.element)
                    .font(
                        .system(size: V4.pairCodeSize, weight: .bold, design: .monospaced)
                    )
                    .tracking(V4.pairCodeTracking)
                    .foregroundStyle(Tok.ink)
                    .padding(.vertical, V4.pairCodePaddingV)
                    .padding(.horizontal, V4.pairCodePaddingH)
                    .overlay(
                        RoundedRectangle(cornerRadius: V4.pairCodeRadius)
                            .strokeBorder(Tok.cardLine, lineWidth: V4.panelBorderWidth)
                    )
            }
        }
        .frame(maxWidth: .infinity, alignment: .center)
        .padding(.top, V4.pairCodeMarginTop)
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(
            "This Mac shows \(code.map(String.init).joined(separator: " "))")
    }

    /// Three at a time, so six digits read as `418 902` and not as one run.
    private func groups(of code: String) -> [String] {
        stride(from: 0, to: code.count, by: V4.pairCodeGroup).map { start in
            let from = code.index(code.startIndex, offsetBy: start)
            let to = code.index(from, offsetBy: min(V4.pairCodeGroup, code.count - start))
            return String(code[from..<to])
        }
    }

    /// Where the OTHER Mac's digits are typed.
    ///
    /// A plain field on the panel's own plate rather than a system text field
    /// in a box: everything else on this sheet is drawn, and a focus ring from
    /// a different design system in the middle of it is the seam.
    @ViewBuilder
    private var comparedField: some View {
        let typed = compare.typed
        VStack(spacing: V4.pairFieldCaptionGap) {
            // The caption, not a placeholder inside the box. A placeholder
            // spelled `000000` is six digits where a value belongs, which is
            // the same defect as the six middle dots this sheet already
            // refuses: it reads as a number that is present.
            Text("What \(peerName) is showing")
                .font(V4.font(V4.muteSize))
                .foregroundStyle(Tok.mute)
            ZStack {
                RoundedRectangle(cornerRadius: V4.pairCodeRadius)
                    .fill(Tok.ink.opacity(V4.buttonFillAlpha))
                    .overlay(
                        RoundedRectangle(cornerRadius: V4.pairCodeRadius)
                            .strokeBorder(Tok.cardLine, lineWidth: V4.panelBorderWidth)
                    )
                if snapshotMode {
                    Text(typed)
                        .font(
                            .system(
                                size: V4.pairFieldSize, weight: .semibold, design: .monospaced)
                        )
                        .tracking(V4.pairFieldTracking)
                        .foregroundStyle(Tok.ink)
                } else {
                    TextField(
                        "",
                        text: Binding(
                            get: { compare.typed },
                            set: { compare.set($0) })
                    )
                    .textFieldStyle(.plain)
                    .multilineTextAlignment(.center)
                    .font(
                        .system(size: V4.pairFieldSize, weight: .semibold, design: .monospaced)
                    )
                    .foregroundStyle(Tok.ink)
                    .onSubmit { if canTrust { onTrust() } }
                }
            }
            .frame(width: V4.pairFieldWidth, height: V4.pairFieldHeight)
        }
        .frame(maxWidth: .infinity, alignment: .center)
        .accessibilityElement(children: .combine)
        .accessibilityLabel("The six digits \(peerName) is showing")
        .accessibilityValue(typed)
    }

    @ViewBuilder
    private var glyph: some View {
        switch state {
        case .asking:
            if snapshotMode {
                Image(systemName: "clock").foregroundStyle(Tok.inkDim)
            } else {
                ProgressView().controlSize(.small)
            }
        case .comparing:
            Image(systemName: "number").foregroundStyle(Tok.unmeasured)
        case .done:
            Image(systemName: "checkmark.seal").foregroundStyle(Tok.ok)
        case .refused:
            Image(systemName: "exclamationmark.triangle").foregroundStyle(Tok.near)
        case .cancelled:
            Image(systemName: "xmark.circle").foregroundStyle(Tok.mute)
        }
    }
}

/// The tab's pill, ``V4Pill``'s exact construction, plus the one role it
/// needs and `V4Pill.Role` does not have.
///
/// The mockup's rule 2 reserves one hue for plaintext crossing a machine
/// boundary: `#c69bdd` dark, `#7d4d96` light. Those are exactly `Tok.unknown`'s
/// two values. The gated palette already carries the hue and
/// `scripts/tcrbar-palette.py` already
/// contrast-checks it in both appearances, so this adds no ungated colour.
///
/// It is a second pill type rather than a fifth `V4Pill.Role` case because
/// `PanelV4/V4Pill.swift` is shared across the whole panel and a shared
/// enum is not the file to reach into for one tab. Same tokens, same geometry,
/// same accessibility shape, so the two cannot drift visually; folding this
/// back into `V4Pill.Role` as `.plaintext` is a one-line follow-up in that
/// file's owner's hands.
struct PeerPill: View {
    enum Role {
        case neutral
        case ok
        case info
        /// Another Mac reads this in plaintext. Spent on nothing else.
        case disclosure

        var tint: Color {
            switch self {
            case .neutral: return Tok.dim
            case .ok: return Tok.ok
            case .info: return Tok.unmeasured
            case .disclosure: return Tok.unknown
            }
        }

        var border: Color {
            switch self {
            case .neutral: return Tok.cardLine
            case .ok, .info, .disclosure: return tint.opacity(V4.pillBorderAlpha)
            }
        }
    }

    let text: String
    var role: Role = .neutral
    /// The sentence behind the word. A pill names a state in ten characters;
    /// the help is where the direction and the verb live.
    var help: String?

    var body: some View {
        Text(text.uppercased())
            .font(V4.font(V4.pillFontSize, .bold))
            .tracking(V4.pillTracking)
            .foregroundStyle(role.tint)
            .lineLimit(1)
            .fixedSize()
            .padding(.vertical, V4.pillPaddingV)
            .padding(.horizontal, V4.pillPaddingH)
            .overlay(
                RoundedRectangle(cornerRadius: V4.pillRadius)
                    .strokeBorder(role.border, lineWidth: V4.pillBorderWidth)
            )
            .accessibilityLabel(text)
            .accessibilityValue(help ?? "")
            .help(help ?? text)
    }
}
