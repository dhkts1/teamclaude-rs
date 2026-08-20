import SwiftUI
import TcrBarCore

/// The header drawn above one ``GroupSection``'s member rows in the accounts
/// list — the section IS the account list now; there is no separate Groups
/// tab to draw it in (see `FleetView`'s doc-comment on the toggle that used
/// to live there). Never collapsible: sectioning only labels and orders,
/// it never hides a row.
struct AccountGroupSectionHeader: View {
    let section: GroupSection
    /// The whole fleet, so each group's own "+ add account…" menu can offer
    /// everyone not already a member of THAT group — not just this section's
    /// members, which are only the accounts sharing this exact set.
    let allAccounts: [Account]
    @ObservedObject var groupController: GroupController
    @Binding var confirmRemoveGroup: String?

    /// `ungrouped` is synthetic — nobody manages it, there is nothing to
    /// remove or add to.
    private var isUngrouped: Bool { section.groups.isEmpty }

    var body: some View {
        VStack(alignment: .leading, spacing: Tok.tightSpacing) {
            HStack(spacing: Tok.tightSpacing) {
                if section.isReserved {
                    Image(systemName: "lock.fill")
                        .font(.caption)
                        .foregroundStyle(Tok.near)
                        .help("Held out of the general pool — reserved.")
                        .accessibilityLabel("Reserved")
                }
                Text(section.header)
                    .font(Tok.secondaryFont.weight(.semibold))
                    .lineLimit(1)
                    .truncationMode(.middle)
                Text("\(section.free)/\(section.total)")
                    .font(Tok.secondaryDigitFont)
                    .foregroundStyle(Tok.color(for: section.free == 0 ? FleetTally.Kind.spent : .ok))
                Spacer(minLength: 0)
                if !isUngrouped {
                    HStack(spacing: Tok.tightSpacing) {
                        // A section whose set has two-plus groups shows the
                        // actions for EACH — one menu per member group, not
                        // one menu for the combined set, since "remove" and
                        // "add" are always operations on a single group.
                        ForEach(section.groups, id: \.self) { group in
                            GroupActionsMenu(
                                group: group,
                                allAccounts: allAccounts,
                                groupController: groupController,
                                confirmRemoveGroup: $confirmRemoveGroup
                            )
                        }
                    }
                }
            }
            if section.groups.contains(where: { groupController.needsRestart($0) }) {
                // No live config reload — a successful mutation changes
                // nothing until the proxy restarts, and this view never
                // offers to restart it (that is the guarded, separate
                // control elsewhere).
                Text("restart the proxy to apply")
                    .font(Tok.detailFont)
                    .foregroundStyle(Tok.near)
            }
            if let failure = section.groups.compactMap({ groupController.failure(for: $0) }).first {
                Text(failure.summary)
                    .font(Tok.detailFont)
                    .foregroundStyle(Tok.spent)
            }
        }
        .accessibilityElement(children: .combine)
    }
}

/// One group's own remove/add actions, drawn once per member group in a
/// section's header. Scoped to a single `group` name — membership is a set,
/// so "add" and "remove" always mean "for this one group", even when the
/// header they sit in names several.
struct GroupActionsMenu: View {
    let group: String
    let allAccounts: [Account]
    @ObservedObject var groupController: GroupController
    @Binding var confirmRemoveGroup: String?

    /// Every account NOT already carrying `group` — computed against the
    /// whole fleet, not the section's own members, since a section's members
    /// are only the accounts sharing its exact combined set, and this menu
    /// needs every current member of `group` alone to exclude correctly.
    private var candidatesToAdd: [Account] {
        let memberNames = Set(
            allAccounts.filter { ($0.groups ?? []).contains(group) }.map(\.name))
        return allAccounts.filter { !memberNames.contains($0.name) }
            .sorted { $0.name < $1.name }
    }

    var body: some View {
        Menu {
            Button("Remove Group \(group)…", role: .destructive) {
                confirmRemoveGroup = group
            }
            Menu("Add account to \(group)…") {
                ForEach(candidatesToAdd, id: \.id) { candidate in
                    Button(candidate.name) {
                        Task { await groupController.add(account: candidate.name, to: group) }
                    }
                }
            }
            .disabled(candidatesToAdd.isEmpty)
        } label: {
            HStack(spacing: Tok.space1) {
                Text(group)
                    .font(Tok.detailFont)
                    .foregroundStyle(.secondary)
                Image(systemName: "ellipsis.circle")
                    .foregroundStyle(.secondary)
            }
        }
        .menuStyle(.borderlessButton)
        .fixedSize()
        .accessibilityLabel("Manage group \(group)")
    }
}

/// The "+ New group…" affordance, shown below the sectioned accounts list —
/// group creation still needs a home now that there is no separate Groups
/// tab to hold it. A group is implicit: it exists only once an account
/// carries the label, so creating one requires choosing a first member;
/// there is no "create an empty group" form.
struct NewGroupControl: View {
    let allAccounts: [Account]
    @ObservedObject var groupController: GroupController

    @State private var formOpen = false
    @State private var name = ""
    @State private var accountName: String?
    @State private var validationError: GroupNameValidation.Failure?

    var body: some View {
        if formOpen {
            VStack(alignment: .leading, spacing: Tok.tightSpacing) {
                TextField("Group name", text: $name)
                    .textFieldStyle(.roundedBorder)
                    .font(Tok.secondaryFont)
                if let validationError {
                    Text(GroupNameValidation.message(for: validationError))
                        .font(Tok.detailFont)
                        .foregroundStyle(Tok.spent)
                }
                Menu {
                    ForEach(allAccounts.sorted(by: { $0.name < $1.name }), id: \.id) { account in
                        Button(account.name) { accountName = account.name }
                    }
                } label: {
                    Text(accountName ?? "Choose an account…")
                        .font(Tok.secondaryFont)
                }
                .menuStyle(.borderlessButton)
                .fixedSize()
                if let name = accountName,
                    let failure = groupController.failure(for: "\(self.name)/\(name)")
                {
                    Text(failure.summary)
                        .font(Tok.detailFont)
                        .foregroundStyle(Tok.spent)
                }
                HStack(spacing: Tok.tightSpacing) {
                    Button("Add") { submit() }
                        .disabled(accountName == nil)
                    Button("Cancel") { close() }
                }
                .buttonStyle(.bordered)
            }
        } else {
            Button("+ New group…") { formOpen = true }
                .buttonStyle(.plain)
                .font(Tok.secondaryFont)
                .foregroundStyle(Tok.accent)
        }
    }

    private func close() {
        formOpen = false
        name = ""
        accountName = nil
        validationError = nil
    }

    private func submit() {
        let trimmed = name.trimmingCharacters(in: .whitespacesAndNewlines)
        if let failure = GroupNameValidation.validate(trimmed) {
            validationError = failure
            return
        }
        guard let accountName else { return }
        validationError = nil
        Task {
            let attempt = await groupController.add(account: accountName, to: trimmed)
            if case .accepted = attempt {
                close()
            }
        }
    }
}
