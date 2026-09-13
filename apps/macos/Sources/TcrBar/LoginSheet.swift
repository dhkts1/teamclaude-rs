import SwiftUI
import TcrBarCore

/// The panel's own sign-in sheet: what `tcr login --non-interactive` is doing,
/// while it does it.
///
/// Takes a ``LoginPhase`` rather than the ``LoginSession`` that produces one.
/// That is what lets `--render-states` draw all four states without spawning a
/// login — the harness renders this view directly with a pinned phase, which is
/// the only way a state like `.failed` gets looked at before it happens to a
/// person.
///
/// Not in `PanelV4/`: that directory is a class-by-class transcription of
/// `docs/design/panel-tabs-mockup.html`, and the mockup has no sheet. This
/// reuses the sheet's components (``V4Button``, ``V4.font``, the `Tok` palette)
/// without claiming to be transcribed from a design that does not cover it.
struct LoginSheet: View {
    let phase: LoginPhase
    /// The authorize URL, for the one recovery this flow needs: the browser did
    /// not come forward, or opened as the wrong user.
    var authorizeURL: URL?
    var onCopyLink: () -> Void = {}
    var onCancel: () -> Void = {}
    var onDone: () -> Void = {}
    /// Draw a still glyph where the spinner goes. `--render-states` only:
    /// `ImageRenderer` rasterises a `ProgressView` as the macOS "prohibited"
    /// placeholder, so a fixture of the waiting states showed a red
    /// crossed-out circle where a person sees motion — a picture of a state
    /// this app never draws. Same switch the panel already threads for the
    /// same reason (``FleetView/snapshotMode``).
    var snapshotMode: Bool = false

    var body: some View {
        VStack(alignment: .leading, spacing: V4.buttonGap) {
            HStack(spacing: V4.rowGap) {
                glyph
                Text(title)
                    .font(V4.font(V4.summarySize, .semibold))
                    .foregroundStyle(Tok.ink)
                    .fixedSize(horizontal: false, vertical: true)
            }
            Text(detail)
                .font(V4.font(V4.dimSize))
                .foregroundStyle(Tok.inkDim)
                .fixedSize(horizontal: false, vertical: true)
                .frame(maxWidth: .infinity, alignment: .leading)

            HStack(spacing: V4.buttonGap) {
                Spacer(minLength: 0)
                if authorizeURL != nil, !phase.isTerminal {
                    V4Button(
                        title: "Copy link",
                        help: "Copy the sign-in link, for when the browser did not come forward."
                    ) { onCopyLink() }
                }
                if phase.isTerminal {
                    V4Button(title: "Done") { onDone() }
                } else {
                    V4Button(title: "Cancel", role: .danger) { onCancel() }
                }
            }
        }
        .padding(.vertical, V4.cardPaddingV)
        .padding(.horizontal, V4.cardPaddingH)
        .frame(width: V4.panelWidth, alignment: .leading)
        .background(Tok.panel)
    }

    /// A spinner while the login is live, a state glyph once it is over. The
    /// spinner is the whole point of the waiting states: the browser is in
    /// front of the panel by then, and coming back to a still sheet with no
    /// motion in it reads as a hang.
    @ViewBuilder
    private var glyph: some View {
        switch phase {
        case .opening, .waitingForBrowser:
            if snapshotMode {
                Image(systemName: "clock")
                    .foregroundStyle(Tok.inkDim)
            } else {
                ProgressView()
                    .controlSize(.small)
            }
        case .saved:
            Image(systemName: "checkmark.circle.fill")
                .foregroundStyle(Tok.ok)
        case .failed:
            Image(systemName: Tok.unreadableGlyph)
                .foregroundStyle(Tok.spent)
        }
    }

    private var title: String {
        switch phase {
        case .opening:
            return "Starting sign-in…"
        case .waitingForBrowser(let email):
            guard let email else { return "Signing in in your browser…" }
            return "Signing in as \(email) in your browser…"
        case .saved(let account):
            return "Signed in as \(account)"
        case .failed:
            return "Sign-in failed"
        }
    }

    private var detail: String {
        switch phase {
        case .opening:
            return "Asking tcr for the sign-in link."
        case .waitingForBrowser:
            return "Finish in the browser window that just opened. Nothing is written until it "
                + "comes back. If no window appeared, copy the link and open it yourself."
        case .saved:
            // Deliberately says what is and is not live: an account added
            // through the running proxy serves immediately, while the panel's
            // own row list is a boot-time snapshot of the config
            // (`CLAUDE.md`'s config-reload rule), the same caveat
            // `RemoveAccountControl` draws for a delete.
            return "The account is saved. It starts serving right away; the list here fills in "
                + "on the next poll."
        case .failed(let reason):
            // tcr's own words, unparaphrased — the same rule every other
            // failure surface in this panel follows.
            return reason
        }
    }
}

/// ``LoginSheet`` bound to a live ``LoginSession``.
///
/// The split is what makes the sheet renderable: `LoginSheet` is a function of
/// a phase and nothing else, and this is the only piece that needs a session to
/// observe. A `@State`-held `ObservableObject` publishes nothing on its own —
/// the `@ObservedObject` here is what actually redraws the sheet as the login
/// moves.
struct LoginSheetHost: View {
    @ObservedObject var session: LoginSession
    var onCopyLink: (URL) -> Void
    var onCancel: () -> Void
    var onDone: () -> Void

    var body: some View {
        LoginSheet(
            phase: session.phase,
            authorizeURL: session.authorizeURL,
            onCopyLink: { if let url = session.authorizeURL { onCopyLink(url) } },
            onCancel: onCancel,
            onDone: onDone
        )
    }
}
