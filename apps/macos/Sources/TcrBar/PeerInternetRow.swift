import SwiftUI
import TcrBarCore

/// The "Reachable from the internet" row and its one state line, plus the
/// transient state Gil asked for.
///
/// A view of its own, rather than rows written inline in
/// ``PeersSettingsPane/thisMac``, for one reason that is about being able to
/// SEE it: `Form(.grouped)` is `NSTableView`-backed and `ImageRenderer` never
/// walks its draw path (`RenderSettings`'s header records both failed
/// attempts), so a control built straight into the pane's `Form` can be
/// pictured only by the window-hosting settings harness, one fixture at a
/// time. As a plain `VStack` it draws inside the pane exactly as before AND
/// rasterises under `--render-states`, which is how each of its states
/// gets a PNG a human can open.
///
/// It owns no state and reads no clock: the state arrives already decided by
/// ``PeerInternetReach/state(on:reading:now:)``, so the picture and the pane
/// cannot disagree about what "on" looks like.
struct PeerInternetRow: View {
    let on: Bool
    let state: PeerInternetReach
    /// The press, with the state it moves TO, the same rule the argv keeps.
    var onPress: (Bool) -> Void = { _ in }
    /// Ask the router again, without touching the switch itself. Shown only
    /// on the two states that ended without a path (``PeerInternetReach/canRetry``).
    var onRetry: () -> Void = {}

    var body: some View {
        // The state line and the retry button sit INSIDE the toggle's label
        // column, with the sub-line above them, rather than as siblings of
        // the toggle: a sibling starts at the row's own left margin while
        // the label above it is indented under the switch, so the row had
        // two left edges. One column, one edge.
        Toggle(isOn: Binding(get: { on }, set: onPress)) {
            VStack(alignment: .leading, spacing: 1) {
                Text("Reachable from the internet")
                Text(PeerInternetReach.rowDetail(on: on))
                    .font(.caption)
                    .foregroundStyle(Tok.inkFaint)
                    .fixedSize(horizontal: false, vertical: true)
                if let line = state.line {
                    Text(line)
                        .font(.caption)
                        // Amber for the miss and for a refused probe; ordinary
                        // text while the router is being asked, because waiting is
                        // not a finding. Colour is the second channel: every one
                        // of these lines says its own state in words first.
                        .foregroundStyle(state.isWarning ? Tok.near : Tok.inkFaint)
                        .fixedSize(horizontal: false, vertical: true)
                        .padding(.top, 4)
                }
                if state.canRetry || state == .retrying {
                    Button(
                        state == .retrying ? "Asking…" : "Ask the router again", action: onRetry
                    )
                    .font(.caption)
                    .disabled(state == .retrying)
                }
            }
        }
    }
}
