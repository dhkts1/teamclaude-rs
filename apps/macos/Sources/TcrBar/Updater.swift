import Combine
import Sparkle
import SwiftUI
import TcrBarCore

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
final class Updater: NSObject, ObservableObject {
    /// Mirrors Sparkle's own gate. It is false while a check is already in
    /// flight, so the button is disabled rather than silently no-op — the same
    /// rule the rest of this panel follows.
    @Published private(set) var canCheckForUpdates = false
    /// The outcome of the last completed check, published from Sparkle's own
    /// delegate callbacks below — never inferred by comparing version strings.
    /// Starts `.unknown`: see ``UpdateState`` for why that is the honest
    /// starting value rather than `.upToDate`.
    @Published private(set) var updateState: UpdateState = .unknown

    /// Implicitly-unwrapped and set after `super.init()`: `self` cannot be
    /// handed to Sparkle as `updaterDelegate` until this instance is fully
    /// initialized, and `SPUStandardUpdaterController` takes its delegate only
    /// at construction — there is no settable property to wire it afterward.
    private var controller: SPUStandardUpdaterController!
    private var observation: AnyCancellable?

    /// `startingUpdater: false` exists for the render harness. Starting the
    /// updater schedules background checks and can put UI on screen; a process
    /// invoked with `--render-states` must do neither.
    init(startingUpdater: Bool = true) {
        super.init()
        controller = SPUStandardUpdaterController(
            startingUpdater: startingUpdater,
            updaterDelegate: self,
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

/// Real Sparkle callbacks, not a stubbed indicator. Each method here does
/// nothing but extract a plain value out of what Sparkle handed it and set
/// ``Updater/updateState`` — the mapping itself lives on ``UpdateState`` in
/// `TcrBarCore` and is unit-tested there; these three methods are not,
/// because `TcrBarCore`'s test target deliberately excludes Sparkle
/// (`Package.swift`), so `SUAppcastItem`/`SPUUpdater` are not constructible
/// from a test. `nonisolated` plus a hop to the main actor because Sparkle
/// does not promise which thread calls a delegate method, matching the
/// existing `canCheckForUpdates` publisher above.
extension Updater: SPUUpdaterDelegate {
    nonisolated func updater(_ updater: SPUUpdater, didFindValidUpdate item: SUAppcastItem) {
        let version = item.displayVersionString
        Task { @MainActor in self.updateState = .available(version: version) }
    }

    nonisolated func updaterDidNotFindUpdate(_ updater: SPUUpdater, error: Error) {
        Task { @MainActor in self.updateState = .upToDate }
    }

    /// Sparkle aborts a check it completed successfully but found nothing for,
    /// reporting `SUError.noUpdateError` — and it does so AFTER
    /// ``updaterDidNotFindUpdate(_:error:)`` has already set `.upToDate`, so a
    /// naive mapping here overwrites the good state with a failure carrying
    /// Sparkle's own cheerful text. That shipped, and rendered in red as
    /// "Update check failed: You're up to date!".
    ///
    /// "No update" is an outcome, not an error. Only a genuine abort is a
    /// failure.
    nonisolated func updater(_ updater: SPUUpdater, didAbortWithError error: Error) {
        if Self.isNoUpdateError(error) {
            Task { @MainActor in self.updateState = .upToDate }
            return
        }
        let message = error.localizedDescription
        Task { @MainActor in self.updateState = .failed(message) }
    }

    /// Whether an abort is Sparkle's benign "there was nothing to install".
    /// Matched on the error domain and code rather than its message, which is
    /// user-facing and localised.
    nonisolated static func isNoUpdateError(_ error: Error) -> Bool {
        let ns = error as NSError
        return ns.domain == SUSparkleErrorDomain && ns.code == Int(SUError.noUpdateError.rawValue)
    }
}
