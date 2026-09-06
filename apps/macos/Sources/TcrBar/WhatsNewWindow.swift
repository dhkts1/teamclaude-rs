import AppKit
import SwiftUI
import TcrBarCore

/// The "What's new in TcrBar X.Y.Z" window — the first real window this app
/// has had; everything else is the popover and a transient menu.
///
/// Built once, lazily, and reused, for the same reason `MenuBarShell` keeps
/// one hosting controller for the popover: a window rebuilt per open loses
/// its size, its scroll position and its key focus, and a window that is
/// `isReleasedWhenClosed` (AppKit's default) is deallocated by the red button
/// and crashes or duplicates on the next open.
///
/// ⌘W is handled by hand. There is no main menu in this app (it is an
/// accessory), so nothing else dispatches the Close key equivalent; a local
/// event monitor, installed while the window is on screen, does it.
@MainActor
final class WhatsNewWindow {
    private let controller: WhatsNewController
    private var window: NSWindow?
    private var keyMonitor: Any?

    init(controller: WhatsNewController) {
        self.controller = controller
    }

    /// Bring the window up, frontmost. The activation call is the same one
    /// `openPanel()` and `Updater.checkForUpdates()` make: without it the
    /// window opens BEHIND whatever the operator is looking at.
    func present() {
        let window = self.window ?? makeWindow()
        self.window = window
        window.title = controller.title
        if #available(macOS 14.0, *) {
            NSApp.activate()
        } else {
            NSApp.activate(ignoringOtherApps: true)
        }
        window.makeKeyAndOrderFront(nil)
        installKeyMonitor()
    }

    func close() {
        window?.performClose(nil)
    }

    private func makeWindow() -> NSWindow {
        let hosting = NSHostingController(
            rootView: WhatsNewView(controller: controller, onClose: { [weak self] in self?.close() }))
        let window = NSWindow(contentViewController: hosting)
        window.styleMask = [.titled, .closable, .resizable, .miniaturizable]
        window.isReleasedWhenClosed = false
        window.setContentSize(NSSize(width: 480, height: 520))
        window.minSize = NSSize(width: 360, height: 280)
        window.center()
        NotificationCenter.default.addObserver(
            forName: NSWindow.willCloseNotification, object: window, queue: .main
        ) { [weak self] _ in
            MainActor.assumeIsolated { self?.removeKeyMonitor() }
        }
        return window
    }

    private func installKeyMonitor() {
        guard keyMonitor == nil else { return }
        keyMonitor = NSEvent.addLocalMonitorForEvents(matching: .keyDown) { [weak self] event in
            guard let self, let window = self.window, window.isKeyWindow,
                event.modifierFlags.intersection(.deviceIndependentFlagsMask) == .command,
                event.charactersIgnoringModifiers == "w"
            else { return event }
            window.performClose(nil)
            return nil
        }
    }

    private func removeKeyMonitor() {
        if let keyMonitor {
            NSEvent.removeMonitor(keyMonitor)
            self.keyMonitor = nil
        }
    }
}

/// The window's content: title row with the GitHub link, the notes (or why
/// there are none), a Close button. Scrolls; Esc closes.
struct WhatsNewView: View {
    @ObservedObject var controller: WhatsNewController
    let onClose: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            HStack(alignment: .firstTextBaseline) {
                Text(controller.title)
                    .font(.system(size: 17, weight: .semibold))
                    .foregroundStyle(Tok.ink)
                Spacer()
                if let page {
                    Link("Open on GitHub", destination: page)
                        .font(Tok.secondaryFont)
                }
            }
            .padding(.horizontal, Tok.space5)
            .padding(.top, Tok.space5)
            .padding(.bottom, Tok.space4)
            Hairline()
            ScrollView {
                VStack(alignment: .leading, spacing: Tok.space3) {
                    content
                }
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(Tok.space5)
            }
            Hairline()
            HStack {
                Spacer()
                Button("Close", action: onClose)
                    .keyboardShortcut(.cancelAction)
            }
            .padding(Tok.space4)
        }
        .background(Tok.panel)
        .frame(minWidth: 360, minHeight: 280)
    }

    private var page: URL? {
        switch controller.state {
        case .notes(_, _, let page), .failed(_, _, let page): return page
        case .idle, .loading: return nil
        }
    }

    @ViewBuilder
    private var content: some View {
        switch controller.state {
        case .idle, .loading:
            HStack(spacing: Tok.space3) {
                ProgressView().controlSize(.small)
                Text("Loading release notes…")
                    .font(Tok.bodyFont)
                    .foregroundStyle(Tok.inkDim)
            }
        case .failed(_, let message, _):
            Label(message, systemImage: "exclamationmark.triangle")
                .font(Tok.bodyFont)
                .foregroundStyle(Tok.spent)
                .fixedSize(horizontal: false, vertical: true)
        case .notes(_, let blocks, _):
            ForEach(Array(blocks.enumerated()), id: \.offset) { _, block in
                blockView(block)
            }
        }
    }

    @ViewBuilder
    private func blockView(_ block: WhatsNewMarkdown.Block) -> some View {
        switch block {
        case .heading(let text):
            Text(WhatsNewMarkdown.inline(text))
                .font(Tok.titleFont)
                .foregroundStyle(Tok.ink)
                .padding(.top, Tok.space2)
        case .bullet(let text):
            HStack(alignment: .firstTextBaseline, spacing: Tok.space3) {
                Text("•").foregroundStyle(Tok.inkDim)
                Text(WhatsNewMarkdown.inline(text))
                    .font(Tok.bodyFont)
                    .foregroundStyle(Tok.ink)
                    .fixedSize(horizontal: false, vertical: true)
                    .textSelection(.enabled)
            }
        case .paragraph(let text):
            Text(WhatsNewMarkdown.inline(text))
                .font(Tok.bodyFont)
                .foregroundStyle(Tok.ink)
                .fixedSize(horizontal: false, vertical: true)
                .textSelection(.enabled)
        }
    }
}
