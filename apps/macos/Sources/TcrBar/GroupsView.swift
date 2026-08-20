import SwiftUI
import TcrBarCore

/// The `Accounts` / `Groups` toggle. Only ever drawn when at least one
/// account carries a label — see `FleetView.fleetRows(_:)`, which gates its
/// presence and forces `Accounts` when there is nothing to switch to.
struct FleetViewModeToggle: View {
    @Binding var mode: FleetViewModePreference.Mode

    var body: some View {
        Picker("View", selection: $mode) {
            Text("Accounts").tag(FleetViewModePreference.Mode.accounts)
            Text("Groups").tag(FleetViewModePreference.Mode.groups)
        }
        .pickerStyle(.segmented)
        .labelsHidden()
        .accessibilityLabel("Fleet view")
    }
}

/// The grouped list: one disclosure row per group (alphabetical, `ungrouped`
/// last, per `Fleet.groupDetails`), plus the "+ New group…" control at the
/// bottom.
struct GroupsListView: View {
    let fleet: Fleet
    @ObservedObject var groupController: GroupController

    /// Explicit user choices override the free-== 0 default. Not reset when
    /// the fleet re-polls, so a group an operator opened stays open across a
    /// refresh.
    @State private var expandedOverrides: [String: Bool] = [:]
    @State private var confirmRemoveGroup: String?
    @State private var newGroupFormOpen = false
    @State private var newGroupName = ""
    @State private var newGroupAccountName: String?
    @State private var newGroupValidationError: GroupNameValidation.Failure?

    var body: some View {
        VStack(alignment: .leading, spacing: Tok.rowSpacing) {
            ForEach(fleet.groupDetails) { detail in
                GroupSectionView(
                    detail: detail,
                    allAccounts: fleet.accounts,
                    groupController: groupController,
                    expandedOverrides: $expandedOverrides,
                    confirmRemoveGroup: $confirmRemoveGroup
                )
            }
            newGroupSection
        }
        // A group is implicit — it exists only once an account carries the
        // label — so removing every member IS removing the group; there is
        // no separate "delete an empty group" concept to confirm. This
        // dialog exists for the destructive, hard-to-undo shape: clearing
        // every member's label in one call.
        .confirmationDialog(
            "Remove the “\(confirmRemoveGroup ?? "")” group?",
            isPresented: Binding(
                get: { confirmRemoveGroup != nil },
                set: { if !$0 { confirmRemoveGroup = nil } }
            ),
            titleVisibility: .visible
        ) {
            Button("Remove Group", role: .destructive) {
                guard let name = confirmRemoveGroup else { return }
                confirmRemoveGroup = nil
                Task { await groupController.removeAll(group: name) }
            }
            Button("Cancel", role: .cancel) { confirmRemoveGroup = nil }
        } message: {
            Text(
                "This removes the label from every member account. The proxy keeps "
                    + "routing the old way until it restarts.")
        }
    }

    @ViewBuilder
    private var newGroupSection: some View {
        if newGroupFormOpen {
            VStack(alignment: .leading, spacing: Tok.tightSpacing) {
                TextField("Group name", text: $newGroupName)
                    .textFieldStyle(.roundedBorder)
                    .font(Tok.secondaryFont)
                if let error = newGroupValidationError {
                    Text(GroupNameValidation.message(for: error))
                        .font(Tok.detailFont)
                        .foregroundStyle(Tok.spent)
                }
                // A group is implicit — it exists only once an account
                // carries it — so creating one requires choosing a first
                // member; there is no "create an empty group" here.
                Menu {
                    ForEach(fleet.accounts.sorted(by: { $0.name < $1.name }), id: \.id) { account in
                        Button(account.name) { newGroupAccountName = account.name }
                    }
                } label: {
                    Text(newGroupAccountName ?? "Choose an account…")
                        .font(Tok.secondaryFont)
                }
                .menuStyle(.borderlessButton)
                .fixedSize()
                if let name = newGroupAccountName,
                    let failure = groupController.failure(for: "\(newGroupName)/\(name)")
                {
                    Text(failure.summary)
                        .font(Tok.detailFont)
                        .foregroundStyle(Tok.spent)
                }
                HStack(spacing: Tok.tightSpacing) {
                    Button("Add") { submitNewGroup() }
                        .disabled(newGroupAccountName == nil)
                    Button("Cancel") { closeNewGroupForm() }
                }
                .buttonStyle(.bordered)
            }
        } else {
            Button("+ New group…") { newGroupFormOpen = true }
                .buttonStyle(.plain)
                .font(Tok.secondaryFont)
                .foregroundStyle(Tok.accent)
        }
    }

    private func closeNewGroupForm() {
        newGroupFormOpen = false
        newGroupName = ""
        newGroupAccountName = nil
        newGroupValidationError = nil
    }

    private func submitNewGroup() {
        let trimmed = newGroupName.trimmingCharacters(in: .whitespacesAndNewlines)
        if let failure = GroupNameValidation.validate(trimmed) {
            newGroupValidationError = failure
            return
        }
        guard let account = newGroupAccountName else { return }
        newGroupValidationError = nil
        Task {
            let attempt = await groupController.add(account: account, to: trimmed)
            if case .accepted = attempt {
                closeNewGroupForm()
            }
        }
    }
}

/// One group's disclosure row, expanded or collapsed.
struct GroupSectionView: View {
    let detail: GroupDetail
    /// The whole fleet, so "+ add account…" can offer everyone not already a
    /// member — including an account from another group, since membership is
    /// a set, not a partition.
    let allAccounts: [Account]
    @ObservedObject var groupController: GroupController
    @Binding var expandedOverrides: [String: Bool]
    @Binding var confirmRemoveGroup: String?

    private var isExpanded: Bool {
        // Collapsed by default, except a starved group starts expanded —
        // that is the one an operator opened the panel for. The default
        // itself lives on the model (`GroupDetail.startsExpanded`) so a test
        // can assert it directly; this only adds the per-open override.
        expandedOverrides[detail.name] ?? detail.startsExpanded
    }

    /// `ungrouped` is synthetic — nobody "removes" it, and there is nothing
    /// to add an account TO, since adding one here would just mean clearing
    /// every label it has, which is not this control's job.
    private var isUngrouped: Bool { detail.name == "ungrouped" }

    private var candidatesToAdd: [Account] {
        let memberNames = Set(detail.members.map(\.name))
        return allAccounts.filter { !memberNames.contains($0.name) }
            .sorted { $0.name < $1.name }
    }

    var body: some View {
        VStack(alignment: .leading, spacing: Tok.tightSpacing) {
            header
            if isExpanded {
                VStack(alignment: .leading, spacing: Tok.rowLineSpacing) {
                    ForEach(detail.members.sorted(by: { $0.name < $1.name }), id: \.id) { member in
                        GroupMemberRow(
                            account: member, group: detail.name, groupController: groupController)
                    }
                    if !isUngrouped {
                        addAccountControl
                    }
                }
                .padding(.leading, Tok.space4)
            }
        }
    }

    private var header: some View {
        VStack(alignment: .leading, spacing: Tok.tightSpacing) {
            HStack(spacing: Tok.tightSpacing) {
                Button {
                    expandedOverrides[detail.name] = !isExpanded
                } label: {
                    Image(systemName: isExpanded ? "chevron.down" : "chevron.right")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
                .buttonStyle(.plain)
                .accessibilityLabel(isExpanded ? "Collapse \(detail.name)" : "Expand \(detail.name)")

                Text(detail.name)
                    .font(Tok.secondaryFont.weight(.semibold))
                    .lineLimit(1)
                    .truncationMode(.middle)
                Text("\(detail.free)/\(detail.total)")
                    .font(Tok.secondaryDigitFont)
                    .foregroundStyle(Tok.color(for: detail.free == 0 ? FleetTally.Kind.spent : .ok))
                Spacer(minLength: 0)
                if !isUngrouped {
                    Menu {
                        Button("Remove Group…", role: .destructive) {
                            confirmRemoveGroup = detail.name
                        }
                    } label: {
                        Image(systemName: "ellipsis.circle")
                            .foregroundStyle(.secondary)
                    }
                    .menuStyle(.borderlessButton)
                    .fixedSize()
                }
            }
            if let statLine = detail.statLine {
                Text(statLine)
                    .font(Tok.secondaryFont)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
                    .truncationMode(.tail)
            }
            // No live config reload — a successful mutation changes nothing
            // until the proxy restarts, and this view never offers to
            // restart it (that is the guarded, separate control elsewhere).
            if groupController.needsRestart(detail.name) {
                Text("restart the proxy to apply")
                    .font(Tok.detailFont)
                    .foregroundStyle(Tok.near)
            }
            if let failure = groupController.failure(for: detail.name) {
                Text(failure.summary)
                    .font(Tok.detailFont)
                    .foregroundStyle(Tok.spent)
            }
        }
        .accessibilityElement(children: .combine)
    }

    @ViewBuilder
    private var addAccountControl: some View {
        if candidatesToAdd.isEmpty {
            Text("+ add account…")
                .font(Tok.secondaryFont)
                .foregroundStyle(.tertiary)
        } else {
            Menu {
                ForEach(candidatesToAdd, id: \.id) { candidate in
                    Button(candidate.name) {
                        Task { await groupController.add(account: candidate.name, to: detail.name) }
                    }
                }
            } label: {
                Text("+ add account…")
                    .font(Tok.secondaryFont)
                    .foregroundStyle(Tok.accent)
            }
            .menuStyle(.borderlessButton)
            .fixedSize()
        }
    }
}

/// One member of an expanded group: name, a quota bar, its percentage, and a
/// remove control.
struct GroupMemberRow: View {
    let account: Account
    let group: String
    @ObservedObject var groupController: GroupController

    /// Same "no reading vs a real one" tint rule `AccountRow` uses for its
    /// composite bar — this is the group view's compact equivalent, one bar
    /// per member rather than two per-window bars.
    private var tint: Color {
        account.hasQuotaEvidence ? Tok.color(for: account.quotaState) : Tok.unmeasured
    }

    private var removeKey: String { "\(group)/\(account.name)" }

    var body: some View {
        VStack(alignment: .leading, spacing: Tok.rowLineSpacing) {
            HStack(spacing: Tok.tightSpacing) {
                Text(account.name)
                    .font(Tok.secondaryFont)
                    .lineLimit(1)
                    .truncationMode(.middle)
                QuotaBar(fraction: account.fiveHour, tint: tint, label: "\(account.name) 5-hour quota")
                    .frame(width: 60)
                Text(QuotaFormat.percent(account.fiveHour))
                    .font(Tok.secondaryDigitFont)
                    .foregroundStyle(.secondary)
                    .frame(width: 34, alignment: .trailing)
                Spacer(minLength: 0)
                Button {
                    Task { await groupController.remove(account: account.name, from: group) }
                } label: {
                    if groupController.isPending(removeKey) {
                        ProgressView().controlSize(.small)
                    } else {
                        Image(systemName: "minus.circle")
                    }
                }
                .buttonStyle(.plain)
                .disabled(groupController.isPending(removeKey))
                .help("Remove \(account.name) from \(group) — tcr group rm \(group) \(account.name)")
                .accessibilityLabel("Remove \(account.name) from \(group)")
            }
            if let failure = groupController.failure(for: removeKey) {
                Text(failure.summary)
                    .font(Tok.detailFont)
                    .foregroundStyle(Tok.spent)
            }
        }
    }
}
