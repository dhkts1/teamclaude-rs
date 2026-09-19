import SwiftUI
import TcrBarCore

/// The five panes.
///
/// `peers` is last deliberately. The two controls an operator needs live on
/// the panel's own Peers tab and everything else
/// here, so this pane is the detail behind a tab rather than a place anyone has
/// to visit, and the sidebar reads in the order a person meets the app.
enum SettingsTab: String, CaseIterable, Identifiable {
    case general
    case menuBar
    case groupsRotation
    case updates
    case peers

    var id: Self { self }

    var title: String {
        switch self {
        case .general: return "General"
        case .menuBar: return "Menu Bar"
        case .groupsRotation: return "Groups & Rotation"
        case .updates: return "Updates"
        case .peers: return "Peers"
        }
    }

    var systemImage: String {
        switch self {
        case .general: return "gearshape"
        case .menuBar: return "menubar.rectangle"
        case .groupsRotation: return "square.grid.2x2"
        case .updates: return "arrow.triangle.2.circlepath"
        // The same SF Symbol `PanelTab.peers` uses, so the sidebar row and
        // the panel tab it explains read as one thing.
        case .peers: return "network"
        }
    }
}

/// Singleton so `SettingsWindowController.show(tab:)` can select a pane from
/// outside the view tree, the same shape `macos-settings-ui`'s own reference
/// uses. `ObservableObject`, not `@Observable` — this package's platform
/// floor is macOS 13 (`Package.swift`), and the macro needs 14.
///
/// `init()` is plain, not `private`, so a test or a harness can construct an
/// independent instance instead of mutating `.shared` in place — used by
/// nothing today (``RenderSettings`` renders each pane's `Form` directly and
/// never routes through this navigation object at all — see that type's own
/// doc-comment for why), kept for the next caller that needs one.
@MainActor
final class SettingsNavigation: ObservableObject {
    static let shared = SettingsNavigation()
    @Published var selectedTab: SettingsTab? = .general

    /// The trusted Mac whose sheet to open once the Peers pane appears, and
    /// `nil` the rest of the time.
    ///
    /// Decision row 13's account card is a control as well as a readout:
    /// clicking "Lent to attic-nuc 20 %" opens THAT Mac's sheet, which lives
    /// in Settings > Peers and not on the panel. Two windows, so the request
    /// has to travel, and it travels on the object that already carries
    /// "which pane", rather than on a second singleton that would then be a
    /// second answer to "where is Settings right now".
    ///
    /// The pane CLEARS it when it opens the sheet. A request left set would
    /// re-open the sheet every time the pane appeared, which is the shape of
    /// bug a stale route always has.
    @Published var peerSheet: String?
}

/// `NavigationSplitView` sidebar + detail, back/forward toolbar navigation —
/// the shape `macos-settings-ui/references/SettingsView.swift:74-88`
/// prescribes and S1/S7 (`docs/design/panel-tabs-review.md`) hold this window
/// to: **only the selected pane renders**, at a minimum 660×540 with a fixed
/// 200pt sidebar.
struct SettingsRootView: View {
    let dependencies: SettingsDependencies

    /// Always passed in, never defaulted: ``SettingsWindowController`` hands over
    /// the shared singleton, the render harness a fresh instance per capture — see
    /// ``SettingsNavigation``'s own doc-comment. A `= .shared` default argument
    /// reads a main-actor singleton from a nonisolated context, which is an error
    /// in the Swift 6 language mode; naming it at the call site costs one word and
    /// shows which callers reach for the singleton.
    @ObservedObject private var navigation: SettingsNavigation
    @State private var navigationHistory: [SettingsTab] = [.general]
    @State private var historyIndex = 0
    @State private var isHistoryNavigation = false

    init(dependencies: SettingsDependencies, navigation: SettingsNavigation) {
        self.dependencies = dependencies
        self.navigation = navigation
    }

    private var activeTab: SettingsTab {
        navigation.selectedTab ?? .general
    }

    var body: some View {
        NavigationSplitView(columnVisibility: .constant(.all)) {
            SettingsSidebarView(selectedTab: $navigation.selectedTab)
                .frame(width: 200)
                .navigationSplitViewColumnWidth(min: 200, ideal: 200, max: 200)
        } detail: {
            // S1's whole point: ONE pane, chosen by `activeTab`, never all
            // seven sections stacked under whichever tab happened to be
            // marked selected.
            SettingsDetailView(tab: activeTab, dependencies: dependencies)
        }
        .navigationTitle("Settings")
        .navigationSplitViewStyle(.balanced)
        .frame(minWidth: 660, minHeight: 540)
        .toolbar {
            ToolbarItemGroup(placement: .navigation) {
                Button {
                    goBack()
                } label: {
                    Image(systemName: "chevron.left")
                }
                .disabled(!canGoBack)
                Button {
                    goForward()
                } label: {
                    Image(systemName: "chevron.right")
                }
                .disabled(!canGoForward)
            }
        }
        .onChange(of: navigation.selectedTab) { _ in recordNavigation() }
    }

    private var canGoBack: Bool { historyIndex > 0 }
    private var canGoForward: Bool { historyIndex < navigationHistory.count - 1 }

    private func goBack() {
        guard canGoBack else { return }
        isHistoryNavigation = true
        historyIndex -= 1
        navigation.selectedTab = navigationHistory[historyIndex]
        DispatchQueue.main.async { isHistoryNavigation = false }
    }

    private func goForward() {
        guard canGoForward else { return }
        isHistoryNavigation = true
        historyIndex += 1
        navigation.selectedTab = navigationHistory[historyIndex]
        DispatchQueue.main.async { isHistoryNavigation = false }
    }

    private func recordNavigation() {
        guard !isHistoryNavigation else { return }
        guard let tab = navigation.selectedTab else { return }
        if navigationHistory.last == tab { return }
        if historyIndex < navigationHistory.count - 1 {
            navigationHistory = Array(navigationHistory.prefix(historyIndex + 1))
        }
        navigationHistory.append(tab)
        historyIndex = navigationHistory.count - 1
    }
}

private struct SettingsSidebarView: View {
    @Binding var selectedTab: SettingsTab?

    var body: some View {
        List(selection: $selectedTab) {
            ForEach(SettingsTab.allCases) { tab in
                Label(tab.title, systemImage: tab.systemImage).tag(tab)
            }
            versionFooter
        }
        .listStyle(.sidebar)
        .navigationTitle("Settings")
    }

    /// `Version 0.2.48 (d3911a9)` / `server e187866` — the same two facts the
    /// panel footer already states, so a reader who opened Settings from the
    /// gear is not left guessing which build they are looking at.
    private var versionFooter: some View {
        VStack(alignment: .leading, spacing: 2) {
            if let label = AppBuild.label {
                Text(label)
            } else {
                Text("TcrBar (development build)")
            }
        }
        .font(.footnote)
        .foregroundStyle(Tok.inkFaint)
        .fontDesign(.monospaced)
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(.horizontal, 6)
        .padding(.vertical, 8)
        .listRowSeparator(.hidden)
    }
}

private struct SettingsDetailView: View {
    let tab: SettingsTab
    let dependencies: SettingsDependencies

    var body: some View {
        Group {
            switch tab {
            case .general:
                GeneralSettingsPane(dependencies: dependencies)
            case .menuBar:
                MenuBarSettingsPane(dependencies: dependencies)
            case .groupsRotation:
                GroupsRotationSettingsPane(dependencies: dependencies)
            case .updates:
                UpdatesSettingsPane(dependencies: dependencies)
            case .peers:
                PeersSettingsPane(dependencies: dependencies)
            }
        }
        .navigationTitle(tab.title)
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
    }
}
