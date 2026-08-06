import SwiftUI
import TcrBarCore

/// The dropdown panel.
///
/// Its one hard rule: never render a blank list. Every state that is not "a fleet
/// decoded cleanly" gets an explicit, distinguishable banner — `tcr` missing, the
/// poll failing (usually: no server), and an offline read whose counters are
/// structurally zero are three different facts, and an operator who cannot tell
/// them apart will misread the panel.
struct FleetView: View {
    @ObservedObject var poller: StatusPoller
    @ObservedObject var server: ServerController

    var body: some View {
        VStack(alignment: .leading, spacing: Tok.rowSpacing) {
            header
            Divider().overlay(Tok.hairline)
            content
            Divider().overlay(Tok.hairline)
            footer
        }
        .padding(Tok.gutter)
        .frame(width: Tok.panelWidth)
        .background(Tok.panel)
    }

    // MARK: Header

    private var header: some View {
        VStack(alignment: .leading, spacing: Tok.tightSpacing) {
            HStack {
                Text("tcr fleet").font(.headline)
                Spacer()
                if let at = poller.lastPollAt {
                    Text(at, style: .time)
                        .font(.caption.monospacedDigit())
                        .foregroundStyle(.secondary)
                }
            }
            Text(poller.state.summary)
                .font(.caption)
                .foregroundStyle(poller.state.isHealthyRead ? .secondary : .primary)
                .fixedSize(horizontal: false, vertical: true)
            if case .loaded(let fleet) = poller.state, let sha = fleet.serverSha {
                Text("server \(sha)\(fleet.serverDirty ? "-dirty" : "")")
                    .font(.caption2.monospaced())
                    .foregroundStyle(.tertiary)
            }
        }
    }

    // MARK: Body

    @ViewBuilder
    private var content: some View {
        switch poller.state {
        case .pending:
            banner(
                icon: "clock",
                title: "Waiting for the first poll",
                detail: "Polling every \(Int(poller.interval))s.",
                tint: Tok.offline
            )
        case .toolMissing(let searched):
            banner(
                icon: Tok.unreadableGlyph,
                title: "tcr is not on PATH",
                detail: "Searched \(searched.count) locations. Set it with "
                    + "`defaults write com.github.dhkts1.tcrbar \(TcrTool.overrideDefaultsKey) <path>`.",
                tint: Tok.spent
            )
        case .commandFailed(let code, let message):
            banner(
                icon: Tok.unreadableGlyph,
                title: "tcr status failed (exit \(code))",
                detail: message.isEmpty ? "The server is probably not running." : message,
                tint: Tok.spent
            )
        case .undecodable(let message):
            banner(
                icon: Tok.unreadableGlyph,
                title: "Unreadable status output",
                detail: message,
                tint: Tok.unknown
            )
        case .loaded(let fleet):
            if fleet.accounts.isEmpty {
                banner(
                    icon: Tok.unreadableGlyph,
                    title: "No accounts configured",
                    detail: "tcr answered, and the fleet is empty.",
                    tint: Tok.unknown
                )
            } else {
                fleetRows(fleet)
            }
        }
    }

    private func fleetRows(_ fleet: Fleet) -> some View {
        VStack(alignment: .leading, spacing: Tok.rowSpacing) {
            if fleet.source.countersAreStructural {
                offlineNotice(fleet.source)
            }
            ScrollView {
                VStack(alignment: .leading, spacing: Tok.rowSpacing) {
                    ForEach(fleet.accounts.sorted(by: { $0.priority < $1.priority })) { account in
                        AccountRow(account: account, countersAreStructural: fleet.source.countersAreStructural)
                    }
                }
            }
            .frame(maxHeight: Tok.panelMaxHeight)
        }
    }

    private func offlineNotice(_ source: StatusSource) -> some View {
        HStack(spacing: Tok.tightSpacing) {
            Image(systemName: "wifi.slash")
            Text("source: \(source.token) — quota is real, all serving counters are structurally zero.")
                .fixedSize(horizontal: false, vertical: true)
        }
        .font(.caption)
        .foregroundStyle(Tok.offline)
    }

    private func banner(icon: String, title: String, detail: String, tint: Color) -> some View {
        HStack(alignment: .top, spacing: Tok.gutter) {
            Image(systemName: icon).foregroundStyle(tint)
            VStack(alignment: .leading, spacing: Tok.tightSpacing) {
                Text(title).font(.subheadline.weight(.semibold))
                Text(detail)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
        }
        .padding(.vertical, Tok.tightSpacing)
    }

    // MARK: Footer

    private var footer: some View {
        VStack(alignment: .leading, spacing: Tok.tightSpacing) {
            Text(server.state.summary)
                .font(.caption)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
            HStack {
                if server.state.isOurChild {
                    Button("Stop server") { server.stop() }
                } else {
                    Button("Start server") { server.start() }
                }
                Button("Refresh") { Task { await poller.pollOnce() } }
                Spacer()
                Button("Quit") { NSApplication.shared.terminate(nil) }
            }
            .buttonStyle(.bordered)
        }
    }
}

/// One account.
struct AccountRow: View {
    let account: Account
    let countersAreStructural: Bool

    var body: some View {
        VStack(alignment: .leading, spacing: Tok.tightSpacing) {
            HStack(spacing: Tok.tightSpacing) {
                Text(account.name)
                    .font(.system(.body, design: .rounded).weight(.medium))
                    .foregroundStyle(account.disabled ? Tok.disabled : .primary)
                    .lineLimit(1)
                    .truncationMode(.middle)
                Spacer(minLength: Tok.tightSpacing)
                if account.disabled { pill("disabled", tint: Tok.disabled) }
                pill(account.quotaState.token, tint: Tok.color(for: account.quotaState))
            }
            QuotaBar(fraction: account.quota, tint: Tok.color(for: account.quotaState))
            HStack(spacing: Tok.tightSpacing) {
                Text(percent(account.quota))
                    .font(.caption.monospacedDigit())
                    .foregroundStyle(.secondary)
                Text("· 5h \(percent(account.fiveHour)) · 7d \(percent(account.sevenDay))")
                    .font(.caption.monospacedDigit())
                    .foregroundStyle(.secondary)
                Spacer()
                Text(account.status)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
            }
            if let hold = account.soonestHold {
                Label(hold.countdownLabel, systemImage: "hourglass")
                    .font(.caption)
                    .foregroundStyle(Tok.near)
            }
            if !countersAreStructural {
                Text("\(account.requests) req · cache \(cacheLabel)")
                    .font(.caption2.monospacedDigit())
                    .foregroundStyle(.tertiary)
            }
            if let error = account.lastStreamError, !error.isEmpty {
                Text("\(account.streamErrorCount)× \(error)")
                    .font(.caption2)
                    .foregroundStyle(Tok.spent)
                    .lineLimit(2)
            }
        }
    }

    /// `null` cache ratio means "nothing to divide by" — say so rather than
    /// printing a 0% that reads as a measurement.
    private var cacheLabel: String {
        guard let ratio = account.cacheHitRatio else { return "n/a" }
        return percent(ratio)
    }

    private func percent(_ value: Double) -> String {
        "\(Int((value * 100).rounded()))%"
    }

    private func pill(_ text: String, tint: Color) -> some View {
        Text(text)
            .font(.caption2.weight(.semibold))
            .padding(.horizontal, Tok.pillPaddingH)
            .padding(.vertical, Tok.pillPaddingV)
            .background(tint.opacity(0.18), in: RoundedRectangle(cornerRadius: Tok.pillRadius))
            .foregroundStyle(tint)
    }
}

/// A clamped quota bar. Over-100% clamps visually but the numeric label beside it
/// still shows the real figure.
struct QuotaBar: View {
    let fraction: Double
    let tint: Color

    var body: some View {
        GeometryReader { geo in
            ZStack(alignment: .leading) {
                RoundedRectangle(cornerRadius: Tok.barRadius).fill(Tok.track)
                RoundedRectangle(cornerRadius: Tok.barRadius)
                    .fill(tint)
                    .frame(width: geo.size.width * min(max(fraction, 0), 1))
            }
        }
        .frame(height: Tok.barHeight)
    }
}
