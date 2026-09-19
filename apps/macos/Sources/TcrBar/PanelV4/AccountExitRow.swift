import SwiftUI
import TcrBarCore

/// "Exits from", on an account card.
///
/// One row: a picker naming where this account's requests leave from, a "must"
/// switch that appears only once a Mac is named, and the note that says what
/// the chosen state actually costs. The note is not decoration: the whole
/// reason to pin an exit is to keep one address, and a control that says
/// "must" and then quietly reroutes on a bad day has lied.
struct AccountExitRow: View {
    let account: String
    let exit: AccountExit
    /// The trusted Macs this account may be pinned to, as the operator names
    /// them. Empty means the picker offers only This Mac, which is honest: a
    /// Mac with no trusted peers has nowhere else to exit from.
    var peers: [String] = []
    /// The rows behind those names, for the three readouts that name the
    /// PINNED Mac rather than list the choices.
    ///
    /// A pin is stored as a 52-character wire id, and `AccountExit`'s no-peers
    /// forms print exactly that, by design: resolving one needs this document
    /// and that type does not hold it. Empty here is not a failure, it is the
    /// before state, and the fallback is the masked id rather than the raw one
    /// (this repository is public and so is every screenshot of this picker).
    var peerRows: [PeerListDocument.PeerEntry] = []
    /// Draw a still picker rather than a `Menu`. `ImageRenderer` rasterises a
    /// `Menu` as the macOS "prohibited" placeholder, the limitation
    /// `PeersTabV4.rowMenu` records, so a render run draws the same label
    /// without the menu behind it.
    var snapshotMode: Bool = false
    var onChoose: (AccountExit.Route) -> Void = { _ in }
    var onToggleMust: (Bool) -> Void = { _ in }

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            HStack(spacing: V4.pillGap) {
                Text("Exits from")
                    .font(V4.font(V4.muteSize))
                    .foregroundStyle(Tok.mute)
                    .lineLimit(1)
                    .fixedSize()
                if snapshotMode {
                    pickerLabel
                } else {
                    Menu {
                        Button(AccountExit.Route.local.label) { onChoose(.local) }
                        ForEach(peers, id: \.self) { peer in
                            Button(peer) { onChoose(.via(peer)) }
                        }
                    } label: {
                        pickerLabel
                    }
                    .menuStyle(.borderlessButton)
                    .menuIndicator(.hidden)
                    .fixedSize()
                    .accessibilityLabel("Where \(account) exits from")
                }
                Spacer(minLength: 0)
                if exit.showsMust {
                    Text("must")
                        .font(V4.font(V4.muteSize))
                        .foregroundStyle(Tok.mute)
                    MustSwitch(on: exit.strict) { onToggleMust(!exit.strict) }
                        .accessibilityLabel(
                            "Must exit from \(exit.route.label(peers: peerRows)) for \(account)"
                        )
                }
            }
            // The lead ruled this readout onto the Accounts card, naming the
            // Mac verbatim, and it is drawn on its own line rather than as a
            // pill in the card header. Measured twice, not chosen: in the
            // header at the panel's 372 pt the account's own name clipped to
            // `...` (the defect `V4Row` and `PeersTabV4.rowMenu` both record
            // for a trailing column that gains one more item), and inline
            // beside the picker the words themselves clipped to "waiting for
            // studio-m…". The lead's words are kept whole; only the position
            // moved.
            if let waiting = exit.waitingPill(peers: peerRows) {
                Text(waiting)
                    .font(V4.font(V4.muteSize, .semibold))
                    .foregroundStyle(Tok.near)
                    .lineLimit(1)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .help(
                        "This account is pinned to that Mac and may not use another "
                            + "address, so its requests wait until it is back.")
            }
            if let note = exit.note(peers: peerRows) {
                Text(note)
                    .font(V4.font(V4.muteSize))
                    .foregroundStyle(exit.noteIsWarning ? Tok.near : Tok.mute)
                    .fixedSize(horizontal: false, vertical: true)
                    .frame(maxWidth: .infinity, alignment: .leading)
            }
        }
        .padding(.top, V4.quotaMarginTop)
    }

    /// The label the live `Menu` and its render stand-in share, so the two
    /// cannot draw two different pickers.
    private var pickerLabel: some View {
        HStack(spacing: 4) {
            Text(exit.route.label(peers: peerRows))
                .font(V4.font(V4.muteSize))
                .foregroundStyle(Tok.ink)
                .lineLimit(1)
            Image(systemName: "chevron.down")
                .font(.system(size: V4.muteSize - 2, weight: .semibold))
                .foregroundStyle(Tok.mute)
        }
        .padding(.horizontal, V4.pillPaddingH)
        .padding(.vertical, V4.pillPaddingV)
        .background(
            RoundedRectangle(cornerRadius: 6).fill(Color.primary.opacity(0.06))
        )
        .overlay(
            RoundedRectangle(cornerRadius: 6).strokeBorder(Tok.cardLine, lineWidth: 0.5)
        )
        .contentShape(Rectangle())
    }
}

/// The compact switch the mockup's `.sw2.small` draws, by hand.
///
/// Not a `Toggle(.switch)`: `ImageRenderer` draws one as the "prohibited"
/// placeholder whatever its value is (`RenderStates`'s own header measured it),
/// so the four exits scenes would all have pictured the same grey box where
/// the difference between "soft" and "must" lives.
struct MustSwitch: View {
    let on: Bool
    var press: () -> Void = {}

    var body: some View {
        Button(action: press) {
            Capsule()
                .fill(on ? Tok.ok : Tok.track)
                .frame(width: V4.switchWidth, height: V4.switchHeight)
                .overlay(alignment: on ? .trailing : .leading) {
                    Circle()
                        .fill(.white)
                        .frame(width: V4.switchKnobSize, height: V4.switchKnobSize)
                        .padding(V4.switchKnobInset)
                }
        }
        .buttonStyle(.plain)
        .accessibilityAddTraits(on ? [.isSelected] : [])
    }
}
