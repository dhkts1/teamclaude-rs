import SwiftUI

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
    }
}
