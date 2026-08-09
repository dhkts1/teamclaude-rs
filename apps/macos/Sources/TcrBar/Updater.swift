import Combine
import Sparkle
import SwiftUI

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

    /// `startingUpdater: false` exists for the render harness. Starting the
    /// updater schedules background checks and can put UI on screen; a process
    /// invoked with `--render-states` must do neither.
    init(startingUpdater: Bool = true) {
        controller = SPUStandardUpdaterController(
            startingUpdater: startingUpdater,
            updaterDelegate: nil,
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
