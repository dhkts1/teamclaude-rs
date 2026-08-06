import Combine
import Foundation
import ServiceManagement

/// "Launch at login", backed by `SMAppService.mainApp`.
///
/// The one design rule here: **macOS owns this bit, not us.** A cached `Bool` in
/// `UserDefaults` would be a lie the moment the operator revokes the item in
/// System Settings ▸ General ▸ Login Items — the app would keep drawing a toggle
/// that is on while nothing launches. So every read goes to
/// `SMAppService.mainApp.status`, and `refresh()` is called whenever the panel
/// might be shown.
///
/// Registration is also only durable if the bundle's code identity is stable.
/// An ad-hoc signature changes its cdhash on every rebuild, which leaves a
/// registered login item pointing at an identity that no longer matches — see
/// `scripts/build-tcrbar.sh`, which signs with a real certificate when one is
/// available and warns loudly when it has to fall back.
@MainActor
public final class LoginItem: ObservableObject {
    /// Our own reading of `SMAppService.Status`, so the UI never has to switch
    /// over an Apple enum that may gain cases.
    public enum Status: Equatable {
        /// Registered and will launch at login.
        case enabled
        /// Not registered. The normal "off" state.
        case disabled
        /// Registered, but macOS is holding it until the operator approves it in
        /// System Settings. Drawing this as plain "off" is the dishonest option:
        /// the operator did ask for it, and something is waiting on them.
        case requiresApproval
        /// macOS cannot find the bundle it has on file — typically the app was
        /// moved, or re-signed under a different identity.
        case notFound
        /// A case Apple added after this was written. Surfaced, never guessed at.
        case unrecognised(rawValue: Int)

        /// What the toggle should read as. `requiresApproval` is deliberately
        /// *on*, because the request was made; the detail line carries the rest.
        public var isOn: Bool {
            switch self {
            case .enabled, .requiresApproval: return true
            case .disabled, .notFound, .unrecognised: return false
            }
        }

        /// One line for the panel. `nil` means there is nothing worth saying.
        public var detail: String? {
            switch self {
            case .enabled, .disabled:
                return nil
            case .requiresApproval:
                return "macOS needs approval: System Settings ▸ General ▸ Login Items."
            case .notFound:
                return "macOS cannot find this app's registration — move it back, or toggle off and on."
            case .unrecognised(let raw):
                return "Unrecognised login-item status (\(raw))."
            }
        }
    }

    @Published public private(set) var status: Status = .disabled

    /// The last failure from `register()` / `unregister()`, kept visible rather
    /// than swallowed into a toggle that silently springs back.
    @Published public private(set) var lastError: String?

    public init() {
        refresh()
    }

    /// Pure mapping, kept separate from the live service so it is testable.
    public nonisolated static func classify(_ status: SMAppService.Status) -> Status {
        switch status {
        case .enabled: return .enabled
        case .notRegistered: return .disabled
        case .requiresApproval: return .requiresApproval
        case .notFound: return .notFound
        @unknown default: return .unrecognised(rawValue: status.rawValue)
        }
    }

    /// Re-read the real state from macOS. Cheap; call it freely.
    public func refresh() {
        status = Self.classify(SMAppService.mainApp.status)
    }

    /// Flip the registration to `enabled`, surfacing any error.
    public func set(enabled: Bool) {
        lastError = nil
        do {
            if enabled {
                try SMAppService.mainApp.register()
            } else {
                try SMAppService.mainApp.unregister()
            }
        } catch {
            lastError = error.localizedDescription
        }
        refresh()
    }
}
