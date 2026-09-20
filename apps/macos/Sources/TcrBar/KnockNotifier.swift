import AppKit
import Combine
import TcrBarCore
import UserNotifications

/// Posts one banner when a Mac asks to connect, and opens the Peers tab when
/// somebody clicks it.
///
/// Owned by ``MenuBarShell`` beside ``KnockReader`` and for the same reason:
/// the surface this exists for is the one where the panel is closed, and
/// anything owned by a view would be torn down exactly then.
///
/// WHICH Macs and WHAT to say is ``KnockAnnouncer``, a value with no AppKit in
/// it; this file is the plumbing around that decision and holds no rule of its
/// own.
///
/// # The bundle identifier trap
///
/// `UNUserNotificationCenter.current()` traps, not throws, not returns nil,
/// in a process with no bundle identifier. `--render-states`, `--render-mark`
/// and `--shell-probe` all run the raw `swift build` binary, which has none,
/// so the centre is built LAZILY behind that check and every one of those
/// paths must reach ``deliversNotifications`` `false` and stop there.
/// `--shell-probe` asserts it, because "it did not crash" is not evidence that
/// a lazy guard was ever the thing that stopped it.
///
/// `UserNotifications` is a system framework. No package dependency was added.
@MainActor
final class KnockNotifier: NSObject {
    /// The decision, which is the only thing here with a rule in it.
    private var announcer = KnockAnnouncer()
    /// Whether the panel is up right now, asked at the moment of the read: a
    /// banner is never posted over an open panel.
    private let panelIsOpen: () -> Bool
    /// What a click on the banner does. Injected, so this file neither knows
    /// about `NSPopover` nor can reach one.
    private let openPeersTab: () -> Void
    private var reads: Set<AnyCancellable> = []

    /// Whether the centre has ever been built. Reported by `--shell-probe`:
    /// the render paths must leave it `false`, and a probe that never looked
    /// would confirm nothing about the guard.
    private(set) var builtCentre = false
    /// Whether permission has been asked in this process. The ask is once,
    /// and its answer lives in System Settings from then on.
    private(set) var askedForPermission = false

    /// Whether this process can post at all.
    ///
    /// A bundle identifier is what `UNUserNotificationCenter.current()`
    /// requires, so an unbundled run answers `false` here and nothing below it
    /// ever runs. Not a silent fallback: the bar mark and the card carry the
    /// whole job for a person who never gets a banner, which is the same
    /// arrangement somebody who refuses permission is in.
    var deliversNotifications: Bool { Bundle.main.bundleIdentifier != nil }

    init(panelIsOpen: @escaping () -> Bool, openPeersTab: @escaping () -> Void) {
        self.panelIsOpen = panelIsOpen
        self.openPeersTab = openPeersTab
        super.init()
    }

    /// Watch a reader. Every read is folded in, including the ones that post
    /// nothing: the announcer's memory is built from all of them.
    func watch(_ reader: KnockReader) {
        reader.$knocks
            .sink { [weak self] knocks in
                self?.fold(knocks)
            }
            .store(in: &reads)
    }

    /// Fold one read in and post whatever it asks for.
    ///
    /// Not `private`: `--shell-probe` drives it directly to prove that a read
    /// carrying a knock still constructs no centre in an unbundled process.
    func fold(_ knocks: [PeerKnock]) {
        guard let notice = announcer.announce(knocks: knocks, panelIsOpen: panelIsOpen()) else {
            return
        }
        post(notice)
    }

    /// Ask once, the first time **Find Macs on this network** is switched on,
    /// which is the first moment a knock is possible at all. Asking at launch
    /// would ask about a feature most people never turn on.
    ///
    /// A refusal is silent and nothing nags: no line in Settings says
    /// notifications are off, because that would be a second copy of a switch
    /// that lives in System Settings, and the mark on the bar is unconditional.
    func requestAuthorizationOnce() {
        guard !askedForPermission, deliversNotifications, let centre = centre() else { return }
        askedForPermission = true
        centre.requestAuthorization(options: [.alert]) { _, error in
            guard let error else { return }
            // Surfaced, never swallowed: a refusal is not an error and arrives
            // as `granted == false`, so anything landing here is the ask
            // itself failing and an operator reading the log should see it.
            NSLog("TcrBar: notification permission could not be asked: %@", "\(error)")
        }
    }

    // MARK: - The centre, built no earlier than it must be

    private func centre() -> UNUserNotificationCenter? {
        guard deliversNotifications else { return nil }
        let centre = UNUserNotificationCenter.current()
        if !builtCentre {
            builtCentre = true
            centre.delegate = self
        }
        return centre
    }

    private func post(_ notice: KnockAnnouncer.Notice) {
        guard let centre = centre() else { return }
        let content = UNMutableNotificationContent()
        content.title = notice.title
        content.body = notice.body
        // No sound and no interruption level of its own. A request with ten
        // minutes on it is not an emergency, and sound is a System Settings
        // choice per app that this app does not need to assert.
        let request = UNNotificationRequest(
            identifier: "knock-\(notice.addresses.joined(separator: "-"))",
            content: content,
            trigger: nil)
        centre.add(request) { error in
            guard let error else { return }
            NSLog("TcrBar: a knock notification was not delivered: %@", "\(error)")
        }
    }
}

extension KnockNotifier: UNUserNotificationCenterDelegate {
    /// A click opens the panel on the Peers tab, where the card is, with its
    /// address and both answers on it.
    ///
    /// **No Accept or Ignore button on the banner.** The one press that opens
    /// a window to a stranger's Mac happens on the surface that shows the
    /// address it is opening to, and a banner is exactly where a misclick
    /// lands.
    nonisolated func userNotificationCenter(
        _ center: UNUserNotificationCenter,
        didReceive response: UNNotificationResponse,
        withCompletionHandler completionHandler: @escaping () -> Void
    ) {
        Task { @MainActor in
            openPeersTab()
            completionHandler()
        }
    }

    /// Banners while this app is frontmost are still worth showing: it is a
    /// menu bar app, so "frontmost" usually means its Settings window is open
    /// somewhere, not that anybody is looking at a knock. The panel being open
    /// is the case that suppresses a banner, and that is decided before one is
    /// ever posted.
    nonisolated func userNotificationCenter(
        _ center: UNUserNotificationCenter,
        willPresent notification: UNNotification,
        withCompletionHandler completionHandler: @escaping (UNNotificationPresentationOptions) ->
            Void
    ) {
        completionHandler([.banner, .list])
    }
}
