import SwiftUI
import TcrBarCore

/// "How <Mac> sends", on the lease being edited.
///
/// Two options and THREE states, which is the whole design: the segmented
/// control carries the choice, and the line under it carries whether the key
/// that choice is about is actually live right now. A control alone can only
/// show the first, and after a switch back to "Over this Mac" the borrower's
/// old key is still good for a few minutes (decision row 15: to revoke, the
/// owner stops renewing). Saying nothing there would let an operator believe
/// access ended the instant they clicked.
///
/// A view of its own for the same reason ``PeerInternetRow`` is one: it sits
/// inside a `Form`, which `ImageRenderer` cannot draw, so each of its states
/// gets a PNG only if the control can be rasterised on its own.
struct LendModeControl: View {
    let peer: String
    let mode: LendMode
    /// The renewal status, already decided by
    /// ``PeerLease/handedKeyLine(mode:handedKeyUntil:peer:now:)``. `nil` draws
    /// no line at all, which is the honest state for a serve-mode lease that
    /// never handed a key.
    let status: PeerLease.HandedKeyLine?
    var onChoose: (LendMode) -> Void = { _ in }

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            Text("How \(peer) sends")
                .font(.callout)
                .foregroundStyle(Tok.ink)

            // Two buttons in a track, not `Picker(.segmented)`, and the reason
            // is that this control's whole point is WHICH option is chosen:
            // `ImageRenderer` draws a segmented picker as the macOS
            // "prohibited" placeholder whatever its selection is (the same
            // limitation `RenderStates`'s header records for a `Toggle`), so a
            // scene per state would have pictured three identical grey boxes.
            // Drawn this way it is also the mockup's own `.seg2`: a track, and
            // the chosen half lifted.
            HStack(spacing: 2) {
                ForEach(LendMode.allCases, id: \.self) { choice in
                    Button { onChoose(choice) } label: {
                        Text(choice.label)
                            .font(.caption.weight(.semibold))
                            .foregroundStyle(choice == mode ? Tok.ink : Tok.inkDim)
                            .frame(maxWidth: .infinity, minHeight: 22)
                            .background(
                                RoundedRectangle(cornerRadius: 6)
                                    .fill(choice == mode ? Color.primary.opacity(0.14) : Color.clear)
                            )
                    }
                    .buttonStyle(.plain)
                    .accessibilityAddTraits(choice == mode ? [.isSelected] : [])
                }
            }
            .padding(2)
            .background(
                RoundedRectangle(cornerRadius: 8).fill(Color.primary.opacity(0.05))
            )
            .overlay(
                RoundedRectangle(cornerRadius: 8).strokeBorder(Tok.hairline, lineWidth: 0.5)
            )
            .accessibilityElement(children: .contain)
            .accessibilityLabel("How \(peer) sends its requests")

            Text(PeerLease.modeSentence(mode, peer: peer))
                .font(.caption)
                .foregroundStyle(Tok.inkDim)
                .fixedSize(horizontal: false, vertical: true)
                .frame(maxWidth: .infinity, alignment: .leading)

            if let status {
                HStack(alignment: .firstTextBaseline, spacing: 6) {
                    Circle()
                        .fill(status.winding ? Tok.near : Tok.ok)
                        .frame(width: 6, height: 6)
                    Text(status.text)
                        .font(.caption)
                        .foregroundStyle(status.winding ? Tok.near : Tok.inkDim)
                        .fixedSize(horizontal: false, vertical: true)
                }
                .frame(maxWidth: .infinity, alignment: .leading)
            }
        }
    }
}
