import SwiftUI

/// The panel's four text roles.
///
/// ## Truncation is allowed; losing the content is not
///
/// All four end in `.lineLimit(1)` + `.truncationMode(.tail)`, and until this
/// none of them offered the full value anywhere: `rg '\.help\(' PanelV4/`
/// returned three hits, all on Buttons, and `rg textSelection PanelV4/` returned
/// nothing. The hidden text is the IDENTITY of the thing and no second place on
/// the panel says it — 8 of 8 mono command rows on the Tools tab end in an
/// ellipsis, and `dave@example.com` renders as `dave@exam…`. The approved
/// mockup carries the full command as a `title=` on 7 of 7 mono spans; the
/// transcription dropped the whole hover layer.
///
/// So every role carries `.help(text)` — the pointer route, the mockup's own
/// `title=` — and `.accessibilityValue(text)`, the spoken one. ``MonoText``
/// adds `.textSelection(.enabled)`: a command you cannot read is one thing, a
/// command you cannot copy is another.

/// `.name` — an account or session name. 15 pt / 600 / -0.005em, `ink`.
struct NameText: View {
    let text: String
    var body: some View {
        Text(text)
            .font(V4.font(V4.nameSize, .semibold))
            .tracking(V4.nameTracking)
            .foregroundStyle(Tok.ink)
            .lineLimit(1)
            .truncationMode(.tail)
            .frame(minHeight: V4.lineHeight(V4.nameSize), alignment: .leading)
            .help(text)
            .accessibilityValue(text)
    }
}

/// `.dim` — the secondary line: `repo · model`, the quota grid, a metric line.
struct DimText: View {
    let text: String
    var lineLimit: Int? = 1
    var body: some View {
        Text(text)
            .font(V4.font(V4.dimSize))
            .foregroundStyle(Tok.dim)
            .lineLimit(lineLimit)
            .truncationMode(.tail)
            .frame(minHeight: V4.lineHeight(V4.dimSize), alignment: .leading)
            .help(text)
            .accessibilityValue(text)
    }
}

/// `.mute` — the tertiary line: the plan line, a tool's owner, the footer.
struct MuteText: View {
    let text: String
    var lineLimit: Int? = 1
    var body: some View {
        Text(text)
            .font(V4.font(V4.muteSize))
            .foregroundStyle(Tok.mute)
            .lineLimit(lineLimit)
            .truncationMode(.tail)
            .frame(minHeight: V4.lineHeight(V4.muteSize), alignment: .leading)
            .help(text)
            .accessibilityValue(text)
    }
}

/// `.mono` — a tool call's command head. One line, ellipsised at the tail, and
/// never allowed to push the thing beside it off the row.
struct MonoText: View {
    let text: String
    var body: some View {
        Text(text)
            .font(V4.mono(V4.monoSize))
            .foregroundStyle(Tok.ink)
            .lineLimit(1)
            .truncationMode(.tail)
            .textSelection(.enabled)
            .help(text)
            .accessibilityValue(text)
    }
}
