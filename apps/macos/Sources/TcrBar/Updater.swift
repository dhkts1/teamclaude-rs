import Combine
import Sparkle
import SwiftUI
import TcrBarCore

/// Tells the app that the termination Sparkle is about to perform was asked for.
///
/// Installing an update ends with Sparkle terminating this process and relaunching
/// the new one. `applicationShouldTerminate` refuses terminations nobody asked for
/// — which is what makes the app survive a hidden menu-bar icon — so without this
/// hook that refusal would land on Sparkle and updates would silently never
/// install. Both hooks are implemented because Sparkle can install on quit as well
/// as install-and-relaunch, and only one of the two fires in each case.
///
/// Not `@MainActor`: Sparkle may call these from its own thread and terminates
/// immediately afterwards, so the authorization has to be recorded before the call
/// returns. `TerminationPolicy` is lock-guarded for exactly this.
private final class UpdaterTerminationDelegate: NSObject, SPUUpdaterDelegate {
    func updaterWillRelaunchApplication(_ updater: SPUUpdater) {
        TerminationPolicy.shared.authorize(.updateWillRelaunch)
    }

    func updater(_ updater: SPUUpdater, willInstallUpdate item: SUAppcastItem) {
        TerminationPolicy.shared.authorize(.updateWillRelaunch)
    }
}

/// Self-update, owned by the app the same way `poller` / `server` / `loginItem` /
/// `accounts` are: one instance created in `TcrBarApp` and handed down. Not a
/// singleton — a singleton would start an updater in the PNG-render harness too,
/// which is a process that only asked for bitmaps.
///
/// Sparkle's `SPUStandardUpdaterController` is the whole user-facing flow (check,
/// download, install, relaunch) plus its own UI. It reads `SUFeedURL`,
/// `SUPublicEDKey` and `SUEnableAutomaticChecks` out of the bundle's Info.plist,
/// which `scripts/build-tcrbar.sh` writes — so this class configures nothing and
/// hardcodes no URL. An unsigned or hand-assembled bundle simply has no feed and
/// Sparkle reports that itself rather than being second-guessed here.
///
/// Sparkle orders releases on `CFBundleVersion`, which this project derives from
/// the commit count. `build-tcrbar.sh` documents why a shallow clone is refused:
/// it would make that number go backwards, and an updater comparing versions
/// would conclude there is nothing to install.
@MainActor
final class Updater: ObservableObject {
    /// Mirrors Sparkle's own gate. It is false while a check is already in
    /// flight, so the button is disabled rather than silently no-op — the same
    /// rule the rest of this panel follows.
    @Published private(set) var canCheckForUpdates = false

    private let controller: SPUStandardUpdaterController
    private var observation: AnyCancellable?

    /// Sparkle holds its delegate WEAKLY, so this reference is what keeps the
    /// object alive. Dropped, the update-authorization hooks above would simply
    /// never fire and an update would be refused termination with no error.
    private let terminationDelegate: UpdaterTerminationDelegate

    /// `startingUpdater: false` exists for the render harness. Starting the
    /// updater schedules background checks and can put UI on screen; a process
    /// invoked with `--render-states` must do neither.
    init(startingUpdater: Bool = true) {
        // Built locally first: `self` is not usable as an argument until every
        // stored property is initialized, and `controller` is one of them.
        let terminationDelegate = UpdaterTerminationDelegate()
        self.terminationDelegate = terminationDelegate
        controller = SPUStandardUpdaterController(
            startingUpdater: startingUpdater,
            updaterDelegate: terminationDelegate,
            userDriverDelegate: nil
        )
        observation = controller.updater.publisher(for: \.canCheckForUpdates)
            .receive(on: RunLoop.main)
            .sink { [weak self] value in
                MainActor.assumeIsolated { self?.canCheckForUpdates = value }
            }
    }

    /// The user-initiated check. Sparkle drives every subsequent step, including
    /// telling the user when there is nothing to install — which a background
    /// check deliberately stays silent about.
    func checkForUpdates() {
        controller.updater.checkForUpdates()
    }
}
