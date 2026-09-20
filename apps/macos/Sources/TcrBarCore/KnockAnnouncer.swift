import Foundation

/// Which Macs this app has already told somebody about, and what is left to
/// say on the next read.
///
/// The whole decision, as a value with no `UNUserNotificationCenter`, no
/// `Bundle` and no clock in it: `KnockNotifier` in the app target owns one of
/// these and does nothing but post what it returns. The rules below are each
/// one that was got wrong by something shipped somewhere, and a banner is the
/// one surface in this app that can interrupt a person, so they are tested as
/// arithmetic rather than watched in a menu bar.
///
/// # The five rules
///
/// **Keyed on the ADDRESS, never on the instance id.** A Mac that knocks again
/// gets a new id on the same row (`src/peer/state.rs`), so keying on the id
/// would fire a banner at the knock rate cap, every ten seconds, for one Mac
/// that has asked once.
///
/// **The first read after launch is adopted silently.** A launch that fired
/// three banners for requests from twenty minutes ago teaches a person to
/// ignore the fourth. The bar mark is what covers that case, and it is
/// unconditional.
///
/// **One notification per read, however many arrived in it.** Two Macs
/// knocking inside one three-second window are one banner naming both, not
/// two banners racing each other on screen.
///
/// **Nothing while the panel is open.** The mark follows the STATE and the
/// banner follows the ARRIVAL; with the tab already on screen the card is
/// there to be answered and a banner over it is noise. The address is still
/// recorded as announced, because it has in fact been seen.
///
/// **An address that leaves the pending list is forgotten.** That is what
/// makes a Mac which asks again after being ignored, blocked or left to expire
/// ring a second time, rather than being silent forever because of a decision
/// taken ten minutes ago.
public struct KnockAnnouncer: Equatable, Sendable {
    /// What to post, already worded. `nil` from ``announce(knocks:panelIsOpen:)``
    /// means say nothing at all, which is the answer for most reads.
    public struct Notice: Equatable, Sendable {
        public let title: String
        public let body: String
        /// The addresses this notice is about, in the order they were read.
        /// Carried so a caller can log what it posted rather than re-deriving
        /// it from the sentence.
        public let addresses: [String]
    }

    /// Addresses already told about. Never the instance ids.
    private var announced: Set<String> = []
    /// Whether any read has been folded in yet. The FIRST one is adopted, not
    /// announced, and "no read yet" is a different state from "a read that
    /// found nobody", which is why this is a flag and not `announced.isEmpty`.
    private var adopted = false

    public init() {}

    /// Fold in one read and say what, if anything, to post.
    ///
    /// Mutating rather than a pure function over a returned state: there is
    /// exactly one of these per app and the set it keeps IS the memory the
    /// rules are about. A caller that dropped the result of a pure version
    /// would silently re-announce everything on the next read.
    public mutating func announce(knocks: [PeerKnock], panelIsOpen: Bool) -> Notice? {
        let addresses = knocks.map(\.addr)
        let present = Set(addresses)
        // Forgetting comes FIRST: an address that left the list is forgotten
        // in the same read that a new one arrives, so a Mac which was ignored
        // and asks again in the next three seconds is a new arrival.
        announced.formIntersection(present)

        let fresh = knocks.filter { !announced.contains($0.addr) }
        // Every address in this read is now known about, whatever is posted:
        // the adoption read, the panel-open read and the posted read all
        // record the same thing, so none of them can announce twice.
        announced.formUnion(present)

        guard adopted else {
            adopted = true
            return nil
        }
        guard !panelIsOpen, !fresh.isEmpty else { return nil }
        guard let title = PeerAdmission.knockNoticeTitle(fresh),
            let body = PeerAdmission.knockNoticeBody(fresh)
        else { return nil }
        return Notice(title: title, body: body, addresses: fresh.map(\.addr))
    }

    /// Whether this address has already been announced, for a test and for a
    /// log line. Not a control: nothing outside changes the set.
    public func hasAnnounced(_ address: String) -> Bool { announced.contains(address) }
}
