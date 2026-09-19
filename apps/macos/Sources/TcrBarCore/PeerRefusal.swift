import Foundation

/// The last thing `tcr` refused to do, held until the operator has read it.
///
/// # What it replaces
///
/// A failed verb used to be written into the SNAPSHOT, and the tab drew that
/// snapshot's failure INSTEAD of itself: the Find switch, the mesh, every row
/// and the Share switch disappeared, replaced by one card. Then the three
/// second poll's next read overwrote the snapshot and the card went too. So a
/// refusal destroyed every control on the tab to report itself, and then hid
/// the report about three seconds later, which is the worst possible pairing
/// of those two behaviours. The long refusal a mismatched pairing produces
/// landed here.
///
/// A refusal is therefore NOT part of the read. It is what this panel asked
/// for and was told no about, it lives beside the snapshot rather than in it,
/// and a poll cannot touch it.
///
/// # When it goes
///
/// Two ways, and no third: the operator dismisses it, or the next verb
/// succeeds. It does not expire, because there is no length of time after
/// which an operator who was looking away deserves to be told nothing.
public struct PeerRefusal: Equatable, Sendable {
    /// `tcr`'s own words, unparaphrased. `nil` when there is nothing to show.
    public private(set) var message: String?

    public init(message: String? = nil) {
        self.message = message
    }

    /// A verb was refused. The newest refusal wins: it is the one the
    /// operator just caused.
    public mutating func refused(_ message: String) {
        self.message = message
    }

    /// A verb did what it was asked. Whatever was on screen is answered.
    public mutating func succeeded() {
        message = nil
    }

    /// The operator read it.
    public mutating func dismissed() {
        message = nil
    }

    public var isShowing: Bool { message != nil }
}
