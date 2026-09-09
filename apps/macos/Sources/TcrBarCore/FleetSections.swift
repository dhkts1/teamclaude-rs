/// The panel's account list, cut into real sections: a state band, then a group
/// heading inside it, then the rows.
///
/// This reverses a decision the group plan made three times.
/// `docs/plans/account-groups-plan.md:44-55` (decision \#4, "Row chips, not list
/// sections") rejected sectioning on the grounds that a list cut by group must
/// either duplicate a row for a multi-membership account or invent a "primary"
/// group that the data does not have. The objection was correct and is not
/// retracted here — it is OVERRIDDEN, by Gil, this session, with the cost taken
/// deliberately: an account in two groups gets **two rows**, one under each
/// group heading, and there is no primary-group concept anywhere in this file.
/// The cost is that the row count in the panel is no longer the account count,
/// and an operator reading down the list will see the same email twice. That is
/// the price of not inventing a fact about which group an account "really"
/// belongs to.
///
/// The second decision, also Gil's and also explicit: **state is the outer
/// level, group orders within it.** A group whose accounts are in different
/// states is therefore split across bands and its heading appears more than
/// once. That is truthful — a half-spent group is not one thing — and it is the
/// direct consequence of the panel existing to answer "what can serve me right
/// now", which is the same premise ``Fleet/rowsInDisplayOrder`` sorts on.
///
/// Everything here is pure: a `Fleet` in, an array out, no view state and
/// nothing read from the environment. It lives in the library rather than in
/// `FleetView` for the reason ``PanelHeight`` does — the test target links
/// `TcrBarCore` only, so a view-private helper is a helper nothing can test,
/// and grouping/ordering is exactly the kind of rule that silently drifts.

/// The outer level of the list: what an account can do for you right now.
///
/// Three bands, and the membership test is deliberately NOT
/// ``Account/displayOrder``. That key has seven values and answers "how far
/// down the list does this row go"; a heading needs a coarse answer an operator
/// can read as a sentence. `needsRelogin` and never-probed rows are LIVE here
/// even though `displayOrder` sinks them, because neither is known to be out of
/// tokens and neither was parked by the operator — putting them under "Out of
/// tokens" would state a quota fact this app has not measured, and putting them
/// under "Parked" would blame the operator for a broken credential.
public enum FleetBand: Int, CaseIterable, Equatable, Sendable, Comparable {
    /// Anything that is neither spent nor parked.
    case live = 0
    /// `quotaState == .spent` — the server says the tokens are gone.
    case outOfTokens = 1
    /// The operator took it out of rotation — `disabled` on the row itself, OR
    /// ``Account/isParkedByGroup``.
    ///
    /// Checked FIRST, exactly as ``Account/displayOrder`` checks `disabled`
    /// first: a parked account keeps reporting whatever quota it had when it
    /// left rotation, so a disabled row with a spent quota is parked, not out
    /// of tokens. Reading it the other way would put an operator decision under
    /// a heading that says "wait for the reset".
    ///
    /// **`isParkedByGroup` is in this test on purpose, and it is the one place
    /// this file goes past its brief** (which said the band was `disabled`).
    /// `tcr group park` does not set the account's own `disabled` flag — the
    /// two are deliberately distinct on the wire (`src/stats.rs:108-115`) — but
    /// it does block the account for EVERY request at account level
    /// (`Manager::parked_blocks`, `src/manager/mod.rs:8895-8899`). Classifying
    /// on `disabled` alone would file six accounts of a wholly-parked group
    /// under "Live" while the router sends them nothing, and "live" is the one
    /// word in this panel that must not be guessed: the panel exists to answer
    /// "what can serve me right now".
    case parked = 2

    /// Which band a row belongs to. The whole classification, in one place, so
    /// the section builder and any future caller cannot disagree about it.
    public init(_ account: Account) {
        if account.disabled || account.isParkedByGroup {
            self = .parked
        } else if account.quotaState == .spent {
            self = .outOfTokens
        } else {
            self = .live
        }
    }

    /// The heading text. `outOfTokens` says "Out of tokens" and not "Spent"
    /// because the panel's other spent-facing copy is a countdown to a reset,
    /// and a one-word heading that shares a name with ``QuotaState/spent``
    /// invites a reader to think the band IS that enum case — it is not; a
    /// disabled account can also be spent and lands elsewhere.
    public var title: String {
        switch self {
        case .live: return "Live"
        case .outOfTokens: return "Out of tokens"
        case .parked: return "Parked"
        }
    }

    public static func < (lhs: FleetBand, rhs: FleetBand) -> Bool {
        lhs.rawValue < rhs.rawValue
    }
}

/// The inner level: one group, or the absence of one.
///
/// `ungrouped` is a case rather than an empty string because the two are not
/// the same kind of thing and a string would let a group actually NAMED
/// "" or "Ungrouped" impersonate it.
public enum FleetGroupKey: Hashable, Sendable, Comparable {
    case named(String)
    /// No group label on the wire.
    ///
    /// **This collapses `groups == nil` and `groups == []` into one section, on
    /// purpose.** ``Account/groups`` keeps those distinct — `nil` is "this
    /// server is too old to report groups", `[]` is "reported, and there are
    /// none" — and that distinction stays on the account, where a future
    /// caller can still read it. It does not become a section, because the
    /// section list is a thing an operator ACTS on: against a `tcr` that
    /// predates the field, every row would land under a heading like "Groups
    /// not reported", which is a fact about the server's version rendered as if
    /// it were a property of the accounts, and no click in this panel resolves
    /// it. Both cases render as ungrouped, which is what ``Account/groupTags``
    /// already does with them.
    case ungrouped

    /// The heading text.
    public var title: String {
        switch self {
        case .named(let name): return name
        case .ungrouped: return "Ungrouped"
        }
    }

    /// A collision-free string for identity. Prefixed, so a group literally
    /// named `Ungrouped` cannot produce the same token as ``ungrouped`` — the
    /// exact way a by-name identity collapsed two different rows into one
    /// SwiftUI identity once already (``AccountRef``'s doc-comment has that
    /// story).
    public var token: String {
        switch self {
        case .named(let name): return "g:\(name)"
        case .ungrouped: return "u:"
        }
    }

    /// Named groups sort alphabetically; ungrouped sorts LAST within its band,
    /// because it is the fallback bucket and a reader scanning for a group
    /// heading should not have to step over the unlabelled pile first.
    public static func < (lhs: FleetGroupKey, rhs: FleetGroupKey) -> Bool {
        switch (lhs, rhs) {
        case (.named(let a), .named(let b)): return a < b
        case (.named, .ungrouped): return true
        case (.ungrouped, .named): return false
        case (.ungrouped, .ungrouped): return false
        }
    }
}

/// One account, as it appears under ONE group heading.
///
/// Not an `Account`: the same account is rendered more than once when it is in
/// more than one group, so the thing the view iterates cannot be identified by
/// the account alone. ``id`` is the composite (group, account) identity, and it
/// is a property HERE rather than something `FleetView` synthesises inline,
/// because a `ForEach` identity that collides does not crash — it silently
/// paints one row's data onto another, which is precisely the bug
/// ``AccountRef`` exists to document.
public struct FleetSectionRow: Identifiable, Equatable, Sendable {
    public let account: Account
    /// The group heading this row sits under.
    public let group: FleetGroupKey
    /// The band this row sits under. Derived from `account`, carried anyway so
    /// a caller holding a single row never has to re-derive it and cannot
    /// derive it differently.
    public let band: FleetBand
    /// True when this row is the identity-bound control account
    /// (`tcr control --show`, via `ControlAccountController`).
    public let isControl: Bool

    public init(account: Account, group: FleetGroupKey, band: FleetBand, isControl: Bool) {
        self.account = account
        self.group = group
        self.band = band
        self.isControl = isControl
    }

    /// Stable composite identity: group, then account.
    ///
    /// The separator is ASCII US (`0x1F`), not `|` or `/`: a group name is
    /// operator-typed and an account name already contains `/` on a fleet where
    /// one person holds two orgs (``AccountRef/displayHalves``), so any printable
    /// separator is a character that can appear on both sides of it and let two
    /// different (group, account) pairs produce one string.
    ///
    /// The band is deliberately NOT part of this. Band is a function of the
    /// account, so (group, account) is already unique across the whole list, and
    /// including it would mean a row's identity changes the moment an account
    /// runs out of tokens — SwiftUI would treat that as a delete plus an insert
    /// and animate a row that merely changed color.
    public var id: String { "\(group.token)\u{1F}\(account.id)" }
}

/// One heading and the rows under it.
public struct FleetSection: Identifiable, Equatable, Sendable {
    public let band: FleetBand
    public let group: FleetGroupKey
    /// Already ordered — the caller renders these in array order and derives
    /// nothing.
    public let rows: [FleetSectionRow]

    public init(band: FleetBand, group: FleetGroupKey, rows: [FleetSectionRow]) {
        self.band = band
        self.group = group
        self.rows = rows
    }

    /// The group heading. The band heading is ``FleetBand/title`` and is drawn
    /// once per run of sections sharing a band — see ``isFirstOfBand`` on the
    /// array.
    public var title: String { group.title }

    /// Same construction rule as ``FleetSectionRow/id``, one level up: a group
    /// appears in more than one band whenever its accounts are in different
    /// states, so the band IS part of a section's identity even though it is
    /// not part of a row's.
    public var id: String { "\(band.rawValue)\u{1F}\(group.token)" }

    /// True when any row here is the control account — so a view can decorate
    /// the heading without walking `rows` itself.
    public var containsControl: Bool { rows.contains(where: \.isControl) }
}

extension Array where Element == FleetSection {
    /// Whether the section at `index` opens a new band — the view's cue to draw
    /// a band heading above it. Computed here so the render loop stays a plain
    /// `ForEach` over one array instead of a nested one over a dictionary,
    /// whose iteration order would be unordered by construction.
    public func isFirstOfBand(_ index: Int) -> Bool {
        guard indices.contains(index) else { return false }
        guard index > 0 else { return true }
        return self[index - 1].band != self[index].band
    }
}

extension Fleet {
    /// The whole account list as ordered sections: band, then group, then rows.
    ///
    /// Ordering, top to bottom: ``FleetBand/live``, ``FleetBand/outOfTokens``,
    /// ``FleetBand/parked``; within a band, group sections alphabetically with
    /// ``FleetGroupKey/ungrouped`` last; within a section, accounts by name.
    ///
    /// **By name, not by ``Account/displayOrder``**, and that is a real
    /// difference from ``rowsInDisplayOrder``: inside one band the remaining
    /// `displayOrder` spread is small and sorting by it would reorder a group's
    /// rows on every poll as quotas drift, which reads as flicker in a list
    /// whose headings are stable. The state question is answered by the band
    /// the section is in; within a section the operator is looking a name up,
    /// not ranking.
    ///
    /// `controlName` pins the control account to the front **of each section it
    /// appears in**, not to the front of the list the way
    /// ``rowsInDisplayOrder(pinning:)`` does. A global pin cannot survive
    /// sectioning: hoisting the control row above the first band heading would
    /// put a row outside every group it is in and outside the state band it
    /// belongs to, which is the "primary group" fiction under another name. It
    /// stays reachable instead — first row of its own section(s), and flagged
    /// by ``FleetSectionRow/isControl`` so the view can mark it wherever it
    /// lands. `nil` — no control account, or a build that cannot ask — orders
    /// everything strictly by name.
    ///
    /// An account in two groups yields two rows. A group whose accounts differ
    /// in state yields one section per band. Neither is de-duplicated; see this
    /// file's own header for the override that makes that correct.
    public func sectionsInDisplayOrder(pinning controlName: String? = nil) -> [FleetSection] {
        // One entry per (band, group, account). An account in N groups
        // contributes N of them — the duplication IS the feature, so it happens
        // here, at the point where membership is read, rather than being undone
        // later by anything that looks like de-duplication.
        var buckets: [FleetBand: [FleetGroupKey: [FleetSectionRow]]] = [:]
        for account in accounts {
            let band = FleetBand(account)
            // `nil` and `[]` collapse here, and only here — see
            // ``FleetGroupKey/ungrouped`` for why the distinction stays on the
            // account instead of becoming a section.
            let names = account.groups ?? []
            let keys: [FleetGroupKey] =
                names.isEmpty ? [.ungrouped] : names.map { .named($0) }
            let row = { (key: FleetGroupKey) in
                FleetSectionRow(
                    account: account,
                    group: key,
                    band: band,
                    isControl: controlName != nil && account.name == controlName
                )
            }
            // `Set(keys)` — a wire array that repeats a group label must not
            // produce two identical rows under one heading, which would be a
            // duplicate SwiftUI identity rather than the deliberate duplication
            // across DIFFERENT headings above.
            for key in Set(keys) {
                buckets[band, default: [:]][key, default: []].append(row(key))
            }
        }

        // Empty bands and empty groups are never emitted: the loop walks what
        // the fleet actually produced, not `FleetBand.allCases`, so a fleet with
        // nothing spent draws no "Out of tokens" heading over nothing.
        return buckets.keys.sorted().flatMap { band -> [FleetSection] in
            let groups = buckets[band] ?? [:]
            return groups.keys.sorted().map { key in
                FleetSection(band: band, group: key, rows: sortRows(groups[key] ?? []))
            }
        }
    }

    /// By name, with the control account first. A single `sorted(by:)` on the
    /// pair rather than a partition, because the pin here is inside one section
    /// and there is no pre-existing relative order to preserve — unlike
    /// ``rowsInDisplayOrder(pinning:)``, whose input is already sorted on three
    /// keys it must not disturb.
    private func sortRows(_ rows: [FleetSectionRow]) -> [FleetSectionRow] {
        rows.sorted {
            ($0.isControl ? 0 : 1, $0.account.name) < ($1.isControl ? 0 : 1, $1.account.name)
        }
    }
}
