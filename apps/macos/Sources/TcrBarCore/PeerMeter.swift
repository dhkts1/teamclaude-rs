import CoreGraphics

// Moved out of `PanelV4/PeersTabV4.swift`.
//
// These are pure data (no SwiftUI, no `Tok`, no `V4`), and the test target
// links `TcrBarCore` alone (`Package.swift:39-43`). While they sat in the
// executable target, every assertion about them had to read the SOURCE as
// text; here they are values a test can build and drive.

// MARK: - The two meters, typed apart

/// Which Mac is the lender, said once, so the pill and the meter label that
/// name the same fact cannot drift apart the way `serves`/`serving you` and a
/// meter fixed at `shared` once did: a direction-less word reads the same
/// whether this Mac lends or borrows, so the reader has to guess which.
public enum PeerLendDirection: Equatable {
    /// This Mac answers requests on its own accounts, for the peer.
    case youLend
    /// The peer answers requests on its own accounts, for this Mac.
    case theyLend

    /// The pill's word: present tense, a capability or an act.
    public var pillText: String {
        switch self {
        case .youLend: return "you lend"
        case .theyLend: return "they lend"
        }
    }

    /// The lease meter's word: past tense, what has been spent so far.
    public var meterLabel: String {
        switch self {
        case .youLend: return "you lent"
        case .theyLend: return "they lent"
        }
    }
}

/// A LEASE meter: a fraction of an allowance window, with the sentence that
/// states its scale.
///
/// The mockup's rule 5: "every meter states its scale in the words beside it".
/// The bar is the spend and the words carry the ceiling, because a bar alone
/// has no units.
public struct LeaseFraction: Equatable {
    /// 0 to 1, clamped on construction, a lender that reports 1.4 draws a
    /// full bar, never a bar wider than its track.
    public let spent: Double
    public let sentence: String
    /// `you lent` or `they lent`, the meter label. Defaulted so the tests
    /// that only exercise `spent`/`value` need not name a direction.
    public let label: String
    /// `ends in 1h`, while this lease stops inside the hour
    /// (``PeerLease/endsSoonSeconds``); `nil` at every other distance.
    ///
    /// It takes the meter's right-hand slot FROM the percentage rather than
    /// sitting beside it, because the two answer the same reader at different
    /// moments: how much has been spent, until the answer that matters is how
    /// long is left.
    public let endsIn: String?

    public init(
        spent: Double, sentence: String,
        label: String = PeerLendDirection.youLend.meterLabel, endsIn: String? = nil
    ) {
        self.spent = min(1, max(0, spent))
        self.sentence = sentence
        self.label = label
        self.endsIn = endsIn
    }

    /// `34%` / `nothing yet`, or the countdown while there is one. A Mac that
    /// has stopped serving reads zero, never its last value (rule 5 again).
    public var value: String {
        if let endsIn { return endsIn }
        return spent <= 0 ? "nothing yet" : "\(Int((spent * 100).rounded()))%"
    }

    /// Whether this meter is drawing a countdown, for the one surface that
    /// tints it: the figure and the bar go amber together, and neither
    /// carries the meaning alone.
    public var isEndingSoon: Bool { endsIn != nil }
}

/// A GATEWAY meter: bytes per hour against the hourly ceiling.
///
/// A deliberately different unit from ``LeaseFraction``. Carrying and
/// serving are different acts with different costs, and
/// one meter that could render either would let a blind gateway row claim a
/// share of someone's quota.
public struct GatewayBytes: Equatable {
    public let bytesPerHour: Int64
    public let capBytesPerHour: Int64
    public let sentence: String

    public init(bytesPerHour: Int64, capBytesPerHour: Int64, sentence: String) {
        self.bytesPerHour = bytesPerHour
        self.capBytesPerHour = capBytesPerHour
        self.sentence = sentence
    }

    /// The bar's fill. Zero cap means "no ceiling reported", which draws empty
    /// rather than full: an unmeasured ceiling must not read as "at cap".
    public var fraction: Double {
        guard capBytesPerHour > 0 else { return 0 }
        return min(1, Double(bytesPerHour) / Double(capBytesPerHour))
    }

    public var value: String {
        guard bytesPerHour > 0 else { return "idle" }
        return "\(PeerFormat.megabytes(bytesPerHour)) MB/hr"
    }
}

/// A lease that has ENDED, as the row draws it.
///
/// Decision row 13 keeps an ended lease rather than deleting it, so the
/// operator sees what was lent and can put it back with one click. The tab had
/// no arm for it at all: `isEnded` was consumed in exactly two places, both in
/// the Settings pane, so a Mac whose only lease had ended kept its meter, its
/// pills and a sentence about the offer standing.
///
/// Three strings and an argv, all decided by the builder: the view renders.
public struct LeaseEnded: Equatable {
    /// `22m ago`, or `nil` when the producer said a lease ended without saying
    /// when. Never a guessed time.
    ///
    /// A span and not a wall clock: `ended 09:36` was the only wall clock on
    /// this tab, and with no date on it a lease that ended yesterday read as
    /// one that ended this morning.
    public let when: String?
    public let sentence: String
    /// `tcr peer lend <peer> --relend <id>`, when this Mac is the LENDER and
    /// the record carries a lease id.
    ///
    /// `nil` for a lease this Mac BORROWED: re-lending is the other Mac's act,
    /// and a button that cannot perform it is worse than no button.
    public let relendArguments: [String]?

    public init(when: String?, sentence: String, relendArguments: [String]? = nil) {
        self.when = when
        self.sentence = sentence
        self.relendArguments = relendArguments
    }

    /// `ended 22m ago`, or `ended` when no time was reported.
    public var label: String { when.map { "ended \($0)" } ?? "ended" }
}

/// What a row may draw under its name line.
///
/// A sum type rather than a struct with two optional meters, and that is a
/// type-level unit test: `LeaseMeter` takes a
/// ``LeaseFraction`` and `GatewayMeter` takes a ``GatewayBytes``, neither is
/// convertible to the other, and the only way to reach either view is through
/// the arm that carries its own payload. A gateway row rendering a fraction
/// meter, or a lease row rendering a byte meter, is a compile error rather
/// than a wrong picture, swap the two views' parameter types and this file
/// stops building.
public enum PeerMeter: Equatable {
    /// No meter. The optional string is the row's own sub sentence, what the
    /// row means, when there is no bar to explain.
    case none(String?)
    case lease(LeaseFraction)
    case gateway(GatewayBytes)
    /// The lease is over. No bar at all: a meter reading zero says "nothing is
    /// happening right now", and this row's fact is that nothing will happen
    /// again until somebody lends it back.
    case ended(LeaseEnded)

    /// Whether this row's lease is over. One question, asked of the meter
    /// rather than re-derived, so a row cannot draw a live pill over a dead
    /// meter.
    public var isEnded: Bool {
        if case .ended = self { return true }
        return false
    }

    /// The row SHAPE this meter implies, for ``PeerPanelHeight``.
    public var rowShape: PeerPanelHeight.Row {
        switch self {
        case .none(let sentence):
            return PeerPanelHeight.Row(subLines: sentence == nil ? 1 : 2, hasMeter: false)
        case .lease, .gateway:
            return .metered
        case .ended:
            // The ended line and its sentence, and no bar: the same two sub
            // lines a `.none` row with a sentence charges.
            return PeerPanelHeight.Row(subLines: 2, hasMeter: false)
        }
    }
}
