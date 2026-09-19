import AppKit
import Foundation
import TcrBarCore

/// Serves the whole mesh as a page on this Mac and opens it, for the mini
/// mesh card's "Open full graph" link.
///
/// # Why this is not `PeerController.run(_:)`
///
/// Every other verb this panel runs ends. `tcr peer graph --serve` does not:
/// it binds loopback and keeps serving until it is stopped, so running it
/// through the controller would leave a `Task` waiting on a process that never
/// exits, with that verb stuck in the in-flight set and the switch it guards
/// unpressable for the rest of the session. It is launched and let go instead,
/// and nothing here waits on it.
///
/// It binds LOOPBACK only (`PeerGraphArgs::addr`'s own default and its
/// refusal), so nothing this opens is reachable from another machine.
enum PeerGraphLauncher {
    /// The address `tcr peer graph --serve` binds by default.
    static let url = URL(string: "http://127.0.0.1:7756")

    /// Launch the server, then open the page.
    ///
    /// Returns `tcr`'s own words when the binary cannot be found or the launch
    /// itself fails, and `nil` when the page was opened. A failure here is
    /// surfaced by the caller rather than swallowed: a link that did nothing
    /// and said nothing is the state this app refuses everywhere else.
    @discardableResult
    static func open(
        workspace: NSWorkspace = .shared, launch: (URL, [String]) throws -> Void = runDetached
    ) -> String? {
        switch TcrTool.resolve() {
        case .failure(let notFound):
            return "tcr not found (searched \(notFound.searched.count) locations). "
                + TcrTool.overrideRemedy
        case .success(let executable):
            do {
                try launch(executable, PeerCommand.graphServe)
            } catch {
                return "tcr peer graph --serve could not start: \(error.localizedDescription)"
            }
            guard let url else { return "the graph address could not be built" }
            // The page needs the listener up before it is asked for. A second
            // is the cadence `tcr peer graph --serve` prints its own ready
            // line at; the browser retries anyway, and nothing here waits on
            // the process.
            DispatchQueue.main.asyncAfter(deadline: .now() + 1) {
                workspace.open(url)
            }
            return nil
        }
    }

    /// Start the process and do not wait for it. No pipes: a pipe nobody
    /// drains fills, and a server that has been serving for an hour would
    /// block on its own log.
    static func runDetached(executable: URL, arguments: [String]) throws {
        let process = Process()
        process.executableURL = executable
        process.arguments = arguments
        process.standardOutput = FileHandle.nullDevice
        process.standardError = FileHandle.nullDevice
        try process.run()
    }
}
