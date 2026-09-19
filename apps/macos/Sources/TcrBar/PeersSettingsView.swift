import SwiftUI
import TcrBarCore

/// Settings > Peers, the SHORT pane.
///
/// Gil on the first version of this pane: "way too long, almost unusable". The
/// design is now the Settings > Peers mockup (kept outside the tree) scenes
/// 59 to 64: **three grouped sections and one closed disclosure**, where the
/// first pane had four sections with every control on the top level.
///
/// Nothing was cut. What moved:
///
/// | first pane, top level | now |
/// |---|---|
/// | id, listening, join key, paste a key, regenerate | behind `Advanced…` |
/// | six default-lease numbers | the `Customize…` sheet (scene 62) |
/// | three grant switches per Mac, Forget | that Mac's sheet (scene 63) |
/// | none | a pairing request above the list, and a Blocked list in Advanced |
///
/// That is what keeps the Macs list at one 40 pt row each. The arithmetic is
/// ``PeerPaneLayout`` and the gate is `PeerPaneLayoutTests`.
///
/// **It fits the window, measured, with two points to spare.** The drawn pane
/// is 538 pt at two trusted Macs and one pairing request, against the 540 pt
/// an operator sees in the shipped Settings window, both figures printed by
/// `--render-settings`, and nothing scrolls. It was 675 pt before three changes closed the gap,
/// because a grouped `Form` charges a caption its own row where the mockup
/// nests the sentence inside the row. What paid for the difference, in the
/// order it was measured: the id became the Name row's sub-line (−49),
/// three per-row captions became two head sentences (−64), the pairing
/// sentence went to the tab that already draws it (−30), the Defaults readout
/// went to one line (−24), and the badges came off the heads.
///
/// Two points is the honest margin, not a comfortable one: a third trusted
/// Mac, or one more sentence, puts this pane back into a scroll
/// (`PeerPaneLayoutTests` states the envelope). The levers left are the
/// owner's: a taller Settings window, or two sections instead of three.
///
/// # Two things this pane does NOT do
///
/// **It does not register its rows in ``SettingsRowBadge``.** That table is in
/// `TcrBarCore` and `SettingsRowBadgeTests` gates it in both directions, so
/// adding peer keys means editing that file and its test, which are not owned
/// by this view. The badges below carry a ``SettingsRowTiming`` directly instead.
///
/// **It writes nothing itself.** Every control is argv, the rule
/// ``GroupCommand`` states: this app never edits the peers file in the
/// operator's config directory, because that file is the one that revokes
/// access. Which verbs the `tcr` in
/// this tree does not have yet is stated at each control and summarised in
/// ``PeerLease``'s own header, the scope, the end, and the lease list are
/// fields this build's `tcr` does not yet write, and a control that aimed at
/// a verb this build happens to have would be aiming at the wrong one the
/// moment they land.
struct PeersSettingsPane: View {
    let dependencies: SettingsDependencies
    // **`@StateObject`, never `@ObservedObject`, for a controller this VIEW
    // owns.** `PeersSettingsPane` is a struct, and `SettingsDetailView.body`
    // (`SettingsView.swift`) builds a fresh one, `PeersSettingsPane(dependencies:
    // dependencies)`, on every redraw of the Settings window's detail pane:
    // a sibling tab's state changing, a resize, anything that recomputes that
    // `body`. `@ObservedObject` does not persist an object across that: this
    // `init` ran again each time, `controller` was `nil` again, and a BRAND
    // NEW `PeerController()` replaced the one already running. `onAppear`
    // does not re-fire (the view's identity did not change), so the new
    // controller's own `.start()` never ran and it sat forever at
    // `.snapshot == .empty`, which is what the pane then drew: a briefly
    // healthy Peers pane going blank on an unrelated redraw. The OLD
    // controller's poll loop kept running underneath, unobserved and
    // unstoppable (`onDisappear` never fired either), so every such redraw
    // also leaked one more zombie `tcr peer ls`/`status` polling loop.
    // `@StateObject` ties the instance to the view's own identity instead of
    // to one `init` call, so the SAME controller, and the SAME `knocked` and
    // `pending` sets an in-flight press depends on, survive every redraw.
    @StateObject private var controller: PeerController
    @ObservedObject private var navigation: SettingsNavigation

    init(
        dependencies: SettingsDependencies,
        controller: PeerController? = nil,
        navigation: SettingsNavigation? = nil
    ) {
        self.dependencies = dependencies
        // A render run polls NOTHING. `--render-settings` builds this pane
        // through `SettingsTab.allCases` and cannot hand in a pinned
        // controller, and this pane's own `onAppear` would otherwise shell out
        // to `tcr` from a process whose whole contract is "writes PNGs and
        // exits without polling tcr or touching a server"
        // (`RenderSettings`'s header). Same reasoning as
        // `Updater(startingUpdater: false)` in the other harness: the process
        // was asked for pictures.
        //
        // The `StateObject(wrappedValue:)` autoclosure is what makes this
        // safe under a `@StateObject`: SwiftUI evaluates it only ONCE, the
        // first time this view's identity appears, and ignores it on every
        // later `init` the same identity's redraw runs through. A caller
        // that hands in an explicit `controller` (every test in this file,
        // and every `--render-states` call) still gets exactly that instance,
        // because the first evaluation is also the only one that matters.
        if let controller {
            self._controller = StateObject(wrappedValue: controller)
        } else if RenderSettings.requestedDirectory() != nil {
            self._controller = StateObject(wrappedValue: .pinned(RenderStates.settingsPaneFixture))
        } else {
            self._controller = StateObject(wrappedValue: PeerController())
        }
        self.navigation = navigation ?? SettingsNavigation.shared
    }

    private var snapshot: PeersSnapshot { controller.snapshot }

    /// The instant this pane judges an end against.
    ///
    /// The real clock, except under `--render-settings`, where it is the
    /// fixture's own pinned instant (``RenderStates/peerNow``). Every lease
    /// row asks "has this ended", so a render drawn against the real clock
    /// showed every fixture lease as expired and pictured a scene the design
    /// does not have. One accessor rather than a `Date()` at each call site:
    /// two clocks in one pane can disagree inside a single draw.
    private var now: Date {
        RenderSettings.requestedDirectory() != nil ? RenderStates.peerNow : Date()
    }

    var body: some View {
        Form {
            if snapshot.unsupported {
                Section {
                    Text(
                        "This tcr does not support peers yet. Update it and this pane fills "
                            + "in. Nothing here is written until then."
                    )
                    .font(.callout)
                    .foregroundStyle(Tok.inkDim)
                } header: {
                    Text("Peers")
                }
            } else {
                if let failure = snapshot.failure {
                    // `tcr`'s own words, verbatim and unparaphrased. Before
                    // this the pane ran every verb and drew NOTHING when one
                    // was refused, so a control that could not do what it says
                    // looked exactly like one that had. It is the surface
                    // ``LendModeControl`` needs by name: `--mode hand` is
                    // refused by the CLI until that mode ships, and the
                    // refusal has to be legible rather than swallowed.
                    Section {
                        Text(failure)
                            .font(.callout)
                            .foregroundStyle(Tok.near)
                            .textSelection(.enabled)
                            .fixedSize(horizontal: false, vertical: true)
                    } header: {
                        Text("tcr refused the last change")
                    }
                }
                thisMac
                sharing
                trustedMacs
                advanced
            }
        }
        .formStyle(.grouped)
        .onAppear {
            controller.start()
            fillNameIfEmpty(snapshot.name)
            // The account card's click, arriving from the other window. Taken
            // and CLEARED: a route left set re-opens the sheet on every visit.
            if let requested = navigation.peerSheet {
                navigation.peerSheet = nil
                openSheet(forPeerNamed: requested)
            }
            openRequestedRenderSheet()
        }
        .onDisappear { controller.stop() }
        .onChange(of: snapshot.name) { latest in fillNameIfEmpty(latest) }
        .sheet(isPresented: $showingJoinKey) { joinKeySheet }
        .sheet(isPresented: $pasting) { pasteKeySheet }
        .sheet(isPresented: $customizing) { defaultsSheet }
        .sheet(item: $sheetPeer) { row in macSheet(row) }
        .confirmationDialog(
            "Regenerate this Mac's identity?",
            isPresented: $confirmingRegenerate,
            titleVisibility: .visible
        ) {
            Button("Regenerate identity", role: .destructive) {
                controller.run(PeerCommand.regenerate)
            }
            Button("Cancel", role: .cancel) {}
        } message: {
            Text(
                "\(trustedNames) will stop trusting this Mac. Anything routed through them "
                    + "stops now, and each one has to trust this Mac again. This cannot be "
                    + "undone.")
        }
        .confirmationDialog(
            "Forget this Mac?",
            isPresented: forgettingIsPresented,
            titleVisibility: .visible,
            presenting: forgetting
        ) { row in
            Button("Forget \(row.title)", role: .destructive) {
                controller.run(PeerCommand.forget(peer: row.id))
            }
            Button("Cancel", role: .cancel) {}
        } message: { row in
            Text(
                "\(row.title) can no longer reach this Mac; anything routed through it stops "
                    + "now. Forgetting is per connection, so a Mac reachable through a second "
                    + "trusted Mac keeps that path until that grant goes too.")
        }
        .confirmationDialog(
            "Block this Mac?",
            isPresented: blockingIsPresented,
            titleVisibility: .visible,
            presenting: blocking
        ) { row in
            // The verb takes an ADDRESS (`src/main.rs:194-207`), so a row
            // without one offers no button at all rather than a button aimed
            // at a name the CLI would not match.
            if let address = row.address {
                Button("Block \(address)", role: .destructive) {
                    controller.run(PeerCommand.block(address: address))
                }
            }
            Button("Cancel", role: .cancel) {}
        } message: { row in
            Text(
                "Nothing from \(row.address ?? row.title) is answered again: its address, and "
                    + "its key too once this Mac has learned one. A block does not lift by "
                    + "itself, and Advanced is where it is lifted.")
        }
    }

    /// `confirmationDialog(presenting:)` needs its own `Bool` binding, and
    /// clearing the value on dismiss is what stops a cancelled dialog leaving
    /// the last Mac armed for the next press.
    private var blockingIsPresented: Binding<Bool> {
        Binding(get: { blocking != nil }, set: { if !$0 { blocking = nil } })
    }

    // MARK: - 1. This Mac

    /// Two rows and ONE sentence: the name another Mac shows for this one,
    /// with this Mac's id as its sub-line, and whether that name is
    /// announced.
    ///
    /// # Why the id is here and not a row of its own
    ///
    /// It is the one read-only fact an operator compares against another
    /// screen, and it belongs beside the name because they are the two names
    /// for the same Mac, one the operator chose, one it cannot. As a sub-line
    /// it costs 14 pt; as its own explained row behind Advanced it cost 63,
    /// and an operator hunting for it had to open a disclosure to find out
    /// their Mac has an identity.
    ///
    /// # Why one sentence and not two captions
    ///
    /// The budget forced this. A grouped `Form`
    /// charges a caption its own row (26 pt for two lines), so three
    /// top-level captions were 78 pt of the 675 pt this pane drew into a 500 pt
    /// hole. One sentence per section says the same thing once, and it sits in
    /// the section HEAD because that is the cheapest place on the pane for a
    /// line of prose (``peersSectionHead(_:plaintext:_:)`` carries the three
    /// measurements).
    private var thisMac: some View {
        Section {
            LabeledContent {
                // Commits on return, as every other text field in Settings
                // does. The first pane had a `Set…` button beside it, which
                // is a second control for something the keyboard already
                // does. `.roundedBorder` because a borderless field in a
                // grouped `Form` renders as right-aligned grey text and reads
                // as a LABEL, which is what the rendered pane showed
                // (`/tmp/lan-p2p/settings/peers-dark.png`).
                TextField("Name", text: $editedName, prompt: Text(displayName))
                    .textFieldStyle(.roundedBorder)
                    .labelsHidden()
                    .frame(maxWidth: 180)
                    .onSubmit { commitName() }
            } label: {
                VStack(alignment: .leading, spacing: 0) {
                    Text("Name")
                    Text(nodeId)
                        .font(.caption.monospaced())
                        .foregroundStyle(Tok.inkFaint)
                        .textSelection(.enabled)
                }
            }

            Toggle(isOn: announceNameBinding) {
                Text("Announce my name")
            }

            // Sits under Announce my name because both rows answer "what does
            // this Mac put on a network", one for the office network and one
            // for the open internet.
            PeerInternetRow(on: internetOn, state: internetState, onPress: pressInternet)
        } header: {
            peersSectionHead(
                "This Mac", "Press return to commit. Only the name is announced.")
        }
    }

    // MARK: - 2. Sharing

    /// The switch, and one row that READS OUT the defaults and opens the sheet
    /// that writes them.
    ///
    /// The six numbers themselves are in the sheet (scene 62). Rule 5 of the
    /// mockup is why: a field reading `0.20` is useless without "of each
    /// week's requests" beside it, and that sentence does not fit in 460 px
    /// next to a slider.
    private var sharing: some View {
        Section {
            Toggle(isOn: shareBinding) {
                Text("Share accounts with trusted Macs")
            }

            LabeledContent("Defaults") {
                HStack(spacing: 6) {
                    // ONE line, which is what makes this a 40 pt row: the long
                    // form wrapped to two and cost 64. `PeerLease.defaultsLine`
                    // is the short spelling and `lineLimit(1)` is the promise
                    // that a longer one truncates rather than growing the pane.
                    Text(PeerLease.defaultsLine)
                        .font(.caption.monospacedDigit())
                        .foregroundStyle(Tok.inkDim)
                        .lineLimit(1)
                        .truncationMode(.tail)
                    Button("Customize…") { customizing = true }
                        .controlSize(.small)
                }
            }
        } header: {
            peersSectionHead(
                "Sharing", plaintext: true,
                "A trusted Mac serves your requests in plaintext.")
        }
    }

    // MARK: - 3. Trusted Macs

    /// A pairing request above the list, then one COMPACT row per Mac. Every
    /// per-Mac control is in that Mac's sheet.
    private var trustedMacs: some View {
        Section {
            ForEach(snapshot.pending) { knock in
                knockRow(knock)
            }
            let trusted = snapshot.rows.filter { $0.trust == .trusted }
            if trusted.isEmpty && snapshot.pending.isEmpty {
                Text(
                    "No Mac is trusted yet. A Mac that appears on the Peers tab can do "
                        + "nothing at all until you press Trust on both screens."
                )
                .font(.callout)
                .foregroundStyle(Tok.inkDim)
            }
            ForEach(trusted) { row in
                Button {
                    sheetPeer = row
                } label: {
                    HStack(spacing: 8) {
                        Text(row.title)
                            .foregroundStyle(Tok.ink)
                        Spacer(minLength: 0)
                        ForEach(row.pills, id: \.text) { pill in
                            Text(pill.text)
                                .font(.caption2.weight(.semibold))
                                .foregroundStyle(pillTint(pill.role))
                                .padding(.horizontal, 6)
                                .padding(.vertical, 2)
                                .overlay(
                                    RoundedRectangle(cornerRadius: 6)
                                        .strokeBorder(
                                            pillTint(pill.role).opacity(0.4), lineWidth: 1))
                        }
                        Image(systemName: "chevron.right")
                            .font(.caption.weight(.semibold))
                            .foregroundStyle(Tok.inkFaint)
                    }
                }
                .buttonStyle(.plain)
                .accessibilityLabel("\(row.title). Opens this Mac's settings.")
            }
        } header: {
            Text("Trusted Macs")
        }
    }

    /// `<name> (<addr>) wants to pair`, with Ignore, Block and Accept.
    ///
    /// The same row as the Peers tab's, in this pane's idiom. Both exist
    /// because both are places an operator is looking when somebody knocks,
    /// and the words are ``PeerAdmission``'s so the two cannot phrase one
    /// request two ways.
    /// **One line, not two.** ``PeerAdmission/knockDetail``, "Accepting shows
    /// six digits on both screens; nothing is shared until you press Trust"
    /// is drawn by the Peers TAB, which is where a knock is met: the tab is
    /// what opens when the panel says somebody is waiting, and this pane is
    /// where an operator arrives already knowing. Two copies of the sentence
    /// cost this pane 30 pt of the 175 it had to find, and the three
    /// buttons say what happens.
    private func knockRow(_ knock: PeerKnock) -> some View {
        LabeledContent {
            HStack(spacing: 6) {
                Button("Ignore") {
                    controller.run(PeerCommand.ignore(instance: knock.instanceId))
                }
                .controlSize(.small)
                Button("Block", role: .destructive) {
                    controller.run(PeerCommand.block(instance: knock.instanceId))
                }
                .controlSize(.small)
                Button("Accept") {
                    controller.run(PeerCommand.accept(instance: knock.instanceId))
                }
                .controlSize(.small)
                .keyboardShortcut(.defaultAction)
            }
        } label: {
            Text(PeerAdmission.knockTitle(knock))
                .fontWeight(.semibold)
        }
        .accessibilityHint(PeerAdmission.knockDetail)
    }

    // MARK: - 4. Advanced

    /// Everything almost nobody should change, behind one closed disclosure.
    ///
    /// Closed is the shipped state and it is what makes the pane fit: open, it
    /// runs past the hole and scrolls, which is the trade the operator who
    /// went looking pays (scene 61).
    ///
    /// # Why it stays a card of its own, measured
    ///
    /// Folding it in as the last ROW of Trusted Macs was tried, for the 26 pt
    /// its own chrome and gap cost: the pane got 45 pt TALLER (586 against
    /// 541, `--render-settings`, 2026-09-18). A `DisclosureGroup` sharing a
    /// section with plain rows is charged its own insets on top of theirs. The
    /// cheap-looking structural saving was a loss, and it is recorded here so
    /// nobody pays the build to find out twice.
    private var advanced: some View {
        Section {
            DisclosureGroup(isExpanded: $advancedOpen) {
                LabeledContent("Route the internet through") {
                    Text(via).foregroundStyle(Tok.inkDim)
                }
                peersRow(
                    "auto picks a trusted Mac the moment this one's own route is dead. none "
                        + "never routes through a peer; naming one pins it.")

                LabeledContent("Hop limit") {
                    Text(hopLimit).foregroundStyle(Tok.inkDim)
                }
                peersRow(
                    "1 means a request only ever crosses one other Mac, and nothing chains. "
                        + "0 stops forwarding altogether and makes Forget final everywhere.")

                Toggle(isOn: discoveryBinding) {
                    Text("Announce and look for Macs")
                }
                peersRow(
                    "On whenever Find Macs on this network is on, which is where the switch "
                        + "lives. Off here and only a pasted key can add a Mac.")

                // The id itself is NOT here. It is the sub-line under Name in
                // This Mac, one fact in one place, where the operator
                // comparing it against another screen is already looking.
                // What stays behind Advanced is what the id MEANS, next to
                // the button that replaces it.
                LabeledContent("Listening on") {
                    HStack(spacing: 6) {
                        Text(listenAddress)
                            .font(.caption.monospaced())
                            .foregroundStyle(Tok.inkDim)
                        PeersTag(timing: .boot)
                    }
                }

                LabeledContent("Join key") {
                    Button("Show…") { showJoinKey() }
                        .controlSize(.small)
                }
                peersRow(
                    "For a Mac with no screen, where nobody can compare six digits. One use, "
                        + "ten minutes.")

                LabeledContent("Paste a key or a link") {
                    Button("Paste…") {
                        pastedKey = ""
                        pasting = true
                    }
                    .controlSize(.small)
                }
                peersRow(
                    "The other half of the same act. A whole tcr:// link works here too, it "
                        + "is what opening one in a chat window runs.")

                LabeledContent("Regenerate identity") {
                    Button("Regenerate…", role: .destructive) { confirmingRegenerate = true }
                        .controlSize(.small)
                }
                peersRow(
                    "Mints a new key for this Mac. Every Mac that trusted the old one refuses "
                        + "it, by name, until it is trusted again on both screens. It asks "
                        + "once, and it cannot be undone.")

                capsRows
                blockedList
            } label: {
                Text("Advanced")
            }
        }
    }

    /// What the pane's own height arithmetic says it needs, for the one caller
    /// that can act on a number: the render harness, which grows its window to
    /// hold the pane before capturing it.
    ///
    /// Here rather than in `RenderSettings` because the row COUNTS are this
    /// file's facts, three sections, two explained rows each in the first,
    /// one knock row per request, one compact row per Mac, seven rows behind
    /// the disclosure. ``PeerPaneLayout`` owns the arithmetic and
    /// `PeerPaneLayoutTests` owns the gate; this ties both to the drawn
    /// pane, so the type is not a number only a test knows about.
    static func estimatedHeight(
        trustedMacs: Int, pendingKnocks: Int, advancedOpen: Bool
    ) -> CGFloat {
        let content = shippedContent(
            trustedMacs: trustedMacs, pendingKnocks: pendingKnocks,
            advancedOpen: advancedOpen)
        return PeerPaneLayout.height(content, metrics: shippedMetrics)
    }

    /// The pane's own STRUCTURE, as the arithmetic takes it: no top-level row
    /// carries its own sentence any more, and two section HEADS carry one
    /// (This Mac and Sharing, Trusted Macs is a title alone and Advanced is a
    /// closed disclosure).
    ///
    /// Named here rather than written out at each call site because the two
    /// counts ARE this file's facts, the same way the row counts are, and the
    /// gate reads them from here.
    static func shippedContent(
        trustedMacs: Int, pendingKnocks: Int, advancedOpen: Bool
    ) -> PeerPaneLayout.Content {
        PeerPaneLayout.drawnContent(
            trustedMacs: trustedMacs, pendingKnocks: pendingKnocks,
            advancedOpen: advancedOpen, advancedRows: advancedRowCount)
    }

    /// The height of the pane's LAST row, for the render harness.
    ///
    /// The last row is the `Advanced…` disclosure, and that is the structural
    /// fact the harness needs: a capture that has the bottom of the document
    /// in frame has the disclosure in frame, with no element lookup at all,
    /// which is just as well, because there is no way to do one in that
    /// process (`RenderSettings.scrollTargetHeight(for:)` records both
    /// measurements).
    ///
    /// Scene 59's claim is that this row is reachable without a gesture, so it
    /// is the row a render has to show.
    static var lastRowHeight: CGFloat { shippedMetrics.disclosureHeight }

    /// Seven rows behind the disclosure before the caps and the Blocked list,
    /// which are as long as what `tcr` reported and are therefore not part of
    /// a fixed budget.
    ///
    /// Still seven after the `Id` row moved to the Name row's
    /// sub-line: Route, Hop limit, the discovery switch, Listening on, Join
    /// key, Paste a key, Regenerate.
    static let advancedRowCount = 7

    /// What a grouped `Form` charges this pane, in points, measured off the
    /// rendered capture, and held in `TcrBarCore` so the gate can run it.
    ///
    /// The figures are ``PeerPaneLayout/drawnMetrics``, whose doc-comment
    /// carries the two commands that re-derive them. They are not in this file
    /// because the test target links `TcrBarCore` alone: a copy here would be
    /// the same twelve numbers in two places, and the copy that drifts is the
    /// one nobody runs.
    static let shippedMetrics = PeerPaneLayout.drawnMetrics

    /// The caps, from `tcr peer ls --json`'s own `caps` object.
    ///
    /// **Not six literals.** Decision row 11 shipped these numbers as
    /// constants in `src/peer/{discovery,listener,state}.rs`, and the pane
    /// printing its own copy is the same figure in two places, the copy that
    /// drifts is the one nobody runs. A `tcr` too old to send them says so
    /// rather than showing a number nobody enforces.
    @ViewBuilder private var capsRows: some View {
        if let caps = snapshot.caps {
            ForEach(caps.advancedRows, id: \.label) { row in
                LabeledContent(row.label) {
                    HStack(spacing: 6) {
                        Text(row.value)
                            .font(.caption.monospacedDigit())
                            .foregroundStyle(Tok.inkDim)
                        PeersTag(timing: .readOnly)
                    }
                }
            }
            peersRow(
                "The limits this Mac enforces on strangers, read from tcr itself rather than "
                    + "restated here. They are why a noisy network cannot fill the Peers tab "
                    + "or hold a queue of requests open.")
        } else {
            LabeledContent("Limits") {
                Text("not read yet").foregroundStyle(Tok.inkFaint)
            }
        }
    }

    /// Blocked and muted addresses, each with its reason, and an Unblock.
    ///
    /// Two lists in one place because an operator looking for "why is that Mac
    /// not appearing" does not know which of the two they are in. They are
    /// different states and the rows say which: a mute lifts by itself, a
    /// block does not, and only a block has a control.
    @ViewBuilder private var blockedList: some View {
        if snapshot.blocked.isEmpty && snapshot.muted.isEmpty {
            LabeledContent("Blocked") {
                Text("nobody").foregroundStyle(Tok.inkFaint)
            }
        } else {
            ForEach(snapshot.blocked) { ban in
                LabeledContent {
                    Button("Unblock") {
                        controller.run(PeerCommand.unblock(address: ban.addr))
                    }
                    .controlSize(.small)
                } label: {
                    VStack(alignment: .leading, spacing: 1) {
                        Text(ban.addr)
                            .font(.callout.monospaced())
                        Text(PeerAdmission.blockSentence(ban, now: now))
                            .font(.caption)
                            .foregroundStyle(Tok.inkFaint)
                            .fixedSize(horizontal: false, vertical: true)
                    }
                }
            }
            ForEach(snapshot.muted) { mute in
                LabeledContent {
                    // No control: a mute is an hour of quiet that lifts on its
                    // own (decision row 10). An "unmute" button would be a
                    // control for a state that is already going away, and the
                    // row says when.
                    PeersTag(timing: .readOnly)
                } label: {
                    VStack(alignment: .leading, spacing: 1) {
                        Text(mute.addr)
                            .font(.callout.monospaced())
                        Text("ignored · \(PeerAdmission.muteSentence(mute, now: now))")
                            .font(.caption)
                            .foregroundStyle(Tok.inkFaint)
                    }
                }
            }
            peersRow(
                "A block is forever and is lifted here; an ignore is an hour of quiet that "
                    + "lifts by itself. A block covers the address, and the key too once this "
                    + "Mac has learned one, so a new address does not get that Mac back in.")
        }
    }

    // MARK: - The Sharing defaults sheet (scene 62)

    /// What `Customize…` opens: the scope of the DEFAULT lease, then one
    /// allowance at a time with its unit beside it.
    ///
    /// The scope row is decision row 12's addition and it sits ABOVE the
    /// allowance segments because one scope covers all three of them.
    private var defaultsSheet: some View {
        VStack(alignment: .leading, spacing: 12) {
            VStack(alignment: .leading, spacing: 3) {
                Text("Sharing defaults").font(.headline)
                Text(
                    "What every trusted Mac may spend, unless one of them has its own "
                        + "override."
                )
                .font(.caption)
                .foregroundStyle(Tok.inkDim)
            }

            Form {
                LabeledContent("Lend from") {
                    scopePicker(selection: $defaultScope)
                }
                peersRow("Every trusted Mac's default lease draws from this scope.")

                Picker("Which allowance", selection: $window) {
                    ForEach(PeerLeaseWindow.allCases, id: \.self) { window in
                        Text(window.label).tag(window)
                    }
                }
                .pickerStyle(.segmented)

                LabeledContent("How much of it") {
                    Text(fractionText).foregroundStyle(Tok.inkDim)
                }
                peersRow(
                    "The fraction of the scope's own headroom in this window that one Mac may "
                        + "spend for you. The proxy caps it at 0.50.")

                LabeledContent("For how long") {
                    Text("\(terms.ttlSeconds) s").foregroundStyle(Tok.inkDim)
                }
                peersRow("The ttl. When it lapses the borrowing Mac asks again.")

                LabeledContent("How many at once") {
                    Text("\(terms.maxInFlight)").foregroundStyle(Tok.inkDim)
                }
                peersRow(
                    "max in flight. Not a rate limit: how many of your requests one Mac may "
                        + "have open at the same moment.")

                disclosureNote(
                    "These are what Share accounts writes for every trusted Mac. A Mac "
                        + "serving your request reads it in plaintext; your own sign-in never "
                        + "leaves this Mac. 7d_oi is Fable's own allowance, which is why it "
                        + "is offered separately and defaults to all of it.")
            }
            .formStyle(.grouped)

            HStack(spacing: 8) {
                Spacer(minLength: 0)
                // Cancel writes NOTHING, the mockup's ledger prints the same
                // argv beside it only to show what was not run.
                Button("Cancel", role: .cancel) { customizing = false }
                Button("Done") {
                    controller.run(
                        PeerCommand.shareDefaults(scope: defaultScope, terms: terms))
                    customizing = false
                }
                .keyboardShortcut(.defaultAction)
            }
        }
        .padding(20)
        .frame(width: 412)
    }

    // MARK: - The trusted-Mac sheet (scene 63)

    /// One Mac: its three grants as sentences, its leases, and Forget.
    ///
    /// The three grants' asymmetry is the design's, not a convenience: carrying
    /// is blind and bounded by the byte ceiling, while serving and inspecting
    /// are the two opt-ins, one per side, both default off (decision row 3).
    private func macSheet(_ row: PeerRowModel) -> some View {
        VStack(alignment: .leading, spacing: 12) {
            VStack(alignment: .leading, spacing: 3) {
                HStack(spacing: 6) {
                    Text(row.title).font(.headline)
                    PeersTag(timing: row.awake ? .live : .readOnly)
                }
                Text(row.id)
                    .font(.caption.monospaced())
                    .foregroundStyle(Tok.inkFaint)
                    .textSelection(.enabled)
            }

            Form {
                Section {
                    grant(
                        "\(row.title) may carry your traffic without reading it",
                        detail: "Used when this Mac has no route of its own. It holds your "
                            + "encrypted bytes and cannot open them.",
                        arguments: ["peer", "allow", row.id, "gateway"],
                        plaintext: false)
                    grant(
                        "\(row.title) may serve your requests on its own accounts, and read "
                            + "them",
                        detail: "This is the disclosure. Off is the safe state and the "
                            + "default; the tab's Share switch turns it on for every trusted "
                            + "Mac at once.",
                        arguments: ["peer", "allow", row.id, "disclose"],
                        plaintext: true)
                    grant(
                        "You will read what \(row.title) sends you to serve",
                        detail: "The other direction, and a separate act. On means your own "
                            + "proxy sees \(row.title)'s requests in plaintext while serving "
                            + "them.",
                        arguments: ["peer", "allow", row.id, "inspect"],
                        plaintext: true)
                }

                Section {
                    ForEach(row.lend) { grant in
                        leaseRow(grant, peer: row.id)
                    }
                    Button {
                        // The ellipsis promises further input, so the press
                        // OPENS the sheet and writes nothing. It used to run
                        // `lend --scope all --window 7d` on the press itself,
                        // which granted that Mac a live lease over every
                        // account at the 7-day default and left the operator
                        // narrowing a grant that was already serving.
                        editingLease = .new(peer: row.id)
                    } label: {
                        Label("Add a lease…", systemImage: "plus")
                    }
                    disclosureNote(
                        "Each lease draws only from its own scope, and its share is of that "
                            + "scope's headroom. An end is absolute and separate from the "
                            + "renewal ttl: at the end this Mac stops renewing, the row "
                            + "greys, and Re-lend puts it back. The scope stays on this Mac: "
                            + "what crosses the wire is a window, an amount, the two times "
                            + "and a lease id.")
                } header: {
                    HStack(spacing: 8) {
                        Text("Lend from")
                        if let tag = PeerLease.lendTag(row.lend, now: now) {
                            PeersTag(timing: .live)
                            Text(tag)
                                .font(.caption2)
                                .foregroundStyle(Tok.inkFaint)
                        }
                    }
                }

                Section {
                    LabeledContent("Forget this Mac") {
                        Button("Forget…", role: .destructive) { forgetting = row }
                            .controlSize(.small)
                    }
                    peersRow(
                        "Forget asks once. Forgetting is per connection, so a Mac reachable "
                            + "through a second trusted Mac keeps that path until that grant "
                            + "goes too.")

                    // Block, for a trusted Mac. It used to be reachable only
                    // while a Mac was KNOCKING, so a Mac already trusted could
                    // not be banned at any click depth. It is here rather than
                    // on the tab's trusted row because this sheet is where
                    // every per-Mac act lives, and because that row's trailing
                    // column has no width left: adding a control there clipped
                    // the peer's own name.
                    if let address = row.address {
                        LabeledContent("Block this Mac") {
                            Button("Block…", role: .destructive) { blocking = row }
                                .controlSize(.small)
                        }
                        peersRow(
                            "A stronger act than Forget, and a different one: nothing from "
                                + "\(address) is answered again, its address and its key too "
                                + "once this Mac has learned one. It does not lift by itself. "
                                + "Advanced is where it is lifted.")
                    }
                }
            }
            .formStyle(.grouped)

            HStack {
                Spacer(minLength: 0)
                Button("Done") { sheetPeer = nil }
                    .keyboardShortcut(.defaultAction)
            }
        }
        .padding(20)
        .frame(width: 412)
        // A sheet over this sheet, which is what a lease is: a record inside
        // one Mac's record. Attached here rather than to the pane so it cannot
        // be reached with no Mac in hand.
        .sheet(item: $editingLease) { draft in
            PeerLeaseSheet(
                draft: draft,
                peerLabel: leaseSheetLabel(for: draft),
                groupNames: groupNames,
                accountLabels: accountLabels,
                handedKeyUntil: handedKeyUntil(for: draft),
                now: now,
                onCancel: { editingLease = nil },
                onSave: { edited in
                    controller.run(edited.arguments)
                    editingLease = nil
                })
        }
    }

    /// One lease: two lines, four controls.
    ///
    /// Line one is what it draws from and how much, plus Revoke. Line two is
    /// its END and the sentence that says what that means, `ends 19:00` beside
    /// `in 1 h, renews every 300 s, 2 at once`, because one is the stored fact
    /// and the other is what the operator wants to know, and the renewal ttl is
    /// the figure they would otherwise read as the end.
    ///
    /// An ENDED lease keeps its row, greyed, with Re-lend instead of Revoke:
    /// decision row 13 keeps it so the operator can see what was lent.
    private func leaseRow(_ grant: PeerLendGrant, peer: String) -> some View {
        let ended = grant.isEnded(now: now)
        // ONE writer for the row's three popups. Each of them changes one part
        // of the lease and sends the whole thing, so every one has to carry
        // the other two, and writing that call out three times is how the
        // end silently disappeared on a scope change. A mutation run found
        // exactly that: dropping `end:` from one of the three left the others
        // passing.
        let write: (LendScope, LeaseTerms, LeaseEnd) -> Void = { scope, terms, end in
            controller.run(
                PeerCommand.lend(peer: peer, scope: scope, terms: terms, end: end))
        }
        let scope = grant.scope
        let end = endFor(grant)
        return VStack(alignment: .leading, spacing: 5) {
            HStack(spacing: 7) {
                Menu(grant.scopeLabel) {
                    scopeMenuItems { write($0, grant.terms, end) }
                }
                .disabled(ended)
                .fixedSize()

                Menu(grant.terms.label) {
                    ForEach(PeerLeaseWindow.allCases, id: \.self) { window in
                        Button(LeaseTerms.standard(for: window).label) {
                            write(scope, .standard(for: window), end)
                        }
                    }
                    Divider()
                    // The three above are the shipped defaults for the three
                    // windows, which is all this menu could say: "20 %" was
                    // reachable only because 20 % happens to be the 7-day
                    // default, and "30 % of the work group" could not be
                    // expressed at any depth. Edit… opens the same sheet
                    // Add a lease… opens, on this lease, where the fraction,
                    // the ttl, the in-flight count and the end are all
                    // controls.
                    Button("Edit…") { editingLease = LeaseDraft(editing: grant, peer: peer) }
                }
                .disabled(ended)
                .fixedSize()

                Spacer(minLength: 0)

                if ended {
                    Button("Re-lend") {
                        controller.run(
                            PeerCommand.lendRelend(peer: peer, leaseId: grant.leaseId))
                    }
                    .controlSize(.small)
                } else {
                    Button("Revoke", role: .destructive) {
                        controller.run(
                            PeerCommand.lendRevoke(peer: peer, leaseId: grant.leaseId))
                    }
                    .controlSize(.small)
                }
            }
            HStack(spacing: 7) {
                Menu(grant.endLabel()) {
                    ForEach(endChoices, id: \.label) { choice in
                        Button(choice.label) { write(scope, grant.terms, choice) }
                    }
                }
                .disabled(ended)
                .fixedSize()
                Text(grant.endSentence(now: now))
                    .font(.caption.monospacedDigit())
                    .foregroundStyle(Tok.inkFaint)
            }
        }
        .opacity(ended ? V4.endedRowOpacity : 1)
    }

    /// The three ends decision row 13 draws. `For 2 h` and `Until 18:00` are
    /// the mockup's own two examples; the popup offers them and `No end`,
    /// which is the default.
    private var endChoices: [LeaseEnd] { [.none, .after("2h"), .until("18:00")] }

    /// The end a lease already has, so changing its scope or its numbers does
    /// not silently drop it. One write per lease means every flag goes, and a
    /// missing `--for` is what "no end" means to the CLI, so re-sending the
    /// lease without its end would END the end.
    private func endFor(_ grant: PeerLendGrant) -> LeaseEnd {
        guard let until = grant.until else { return .none }
        return .until(PeerLease.clock(unixSeconds: until))
    }

    /// The scope popup, shared by both sheets.
    ///
    /// `All accounts` first, then every group `tcr group ls` knows, then every
    /// account by its sanitized label. The groups and labels come from the
    /// fleet this window already reads, so the picker cannot offer a group
    /// that does not exist.
    @ViewBuilder private func scopePicker(selection: Binding<LendScope>) -> some View {
        Menu(selection.wrappedValue.label) {
            scopeMenuItems { selection.wrappedValue = $0 }
        }
        .fixedSize()
    }

    /// One definition of the menu, drawn by both sheets and the lease row:
    /// ``PeerScopeMenu``. A second copy here would be a picker free to offer a
    /// group the other one does not.
    @ViewBuilder private func scopeMenuItems(_ choose: @escaping (LendScope) -> Void)
        -> some View
    {
        PeerScopeMenu(
            groupNames: groupNames, accountLabels: accountLabels, choose: choose)
    }

    // MARK: - The two key sheets

    /// Show join key. The sheet is the READOUT of a command's stdout, which is
    /// why it has three states and not one: the invite is still running, it
    /// printed a key, or `tcr` said why it could not mint one.
    private var joinKeySheet: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("Join key")
                .font(.headline)
            switch joinKeyOutcome {
            case nil:
                Text("Minting one, ten minutes and one use.")
                    .font(.callout)
                    .foregroundStyle(Tok.inkDim)
            case .text(let key):
                Text(key)
                    .font(.system(.body, design: .monospaced))
                    .textSelection(.enabled)
                    .fixedSize(horizontal: false, vertical: true)
                    .padding(8)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .background(RoundedRectangle(cornerRadius: 6).fill(Tok.ink.opacity(0.06)))
                    .accessibilityLabel("Join key")
                Text(
                    "Paste it into Settings > Peers on the other Mac, under Paste a key. It "
                        + "works once and expires in ten minutes."
                )
                .font(.caption)
                .foregroundStyle(Tok.inkFaint)
                .fixedSize(horizontal: false, vertical: true)
            case .failed(let message):
                // `tcr`'s own words. No silent fallback: a failed mint says so
                // rather than closing as if it had worked.
                Text(message)
                    .font(.callout)
                    .foregroundStyle(Tok.near)
                    .fixedSize(horizontal: false, vertical: true)
            }
            HStack(spacing: 8) {
                Spacer(minLength: 0)
                if case .text(let key) = joinKeyOutcome {
                    Button("Copy") {
                        NSPasteboard.general.clearContents()
                        NSPasteboard.general.setString(key, forType: .string)
                    }
                }
                Button("Done") { showingJoinKey = false }
                    .keyboardShortcut(.defaultAction)
            }
        }
        .padding(20)
        .frame(width: 380)
    }

    /// Paste a key, or a whole `tcr://` link.
    ///
    /// Either one is fed to `tcr peer join --stdin` on STDIN, never as an
    /// argument. Both are credentials: a join key turns an unknown Mac into a
    /// trusted one, and a link carries the office network key. argv is
    /// readable by every process on this Mac through `ps`, which is the leak
    /// `src/main.rs:501-505` refuses in as many words.
    ///
    /// A pasted LINK is handed over whole. The CLI owns what a link means:
    /// which key sets what, that a spent `jk` still sets `nk`, and re-spelling
    /// its query items here would be a second parser of one string, free to
    /// disagree with the one that ships.
    private var pasteKeySheet: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("Paste a key from another Mac")
                .font(.headline)
            TextField("Join key", text: $pastedKey)
                .textFieldStyle(.roundedBorder)
                .font(.system(.body, design: .monospaced))
            Text(
                "The string the other Mac showed under Show join key, or a whole tcr:// link "
                    + "somebody sent you. Joining with a key trusts that Mac without "
                    + "comparing six digits, which is the whole point of a key: it is for the "
                    + "Mac nobody is looking at."
            )
            .font(.caption)
            .foregroundStyle(Tok.inkFaint)
            .fixedSize(horizontal: false, vertical: true)
            HStack(spacing: 8) {
                Spacer(minLength: 0)
                Button("Cancel", role: .cancel) { pasting = false }
                Button("Join") {
                    controller.run(PeerCommand.join(key: trimmedPastedKey))
                    pasting = false
                }
                .keyboardShortcut(.defaultAction)
                .disabled(trimmedPastedKey.isEmpty)
            }
        }
        .padding(20)
        .frame(width: 380)
    }

    /// Opens the sheet FIRST and fills it in when the invite answers, so a
    /// slow `tcr` reads as a sheet that is working rather than as a button
    /// that did nothing.
    private func showJoinKey() {
        joinKeyOutcome = nil
        showingJoinKey = true
        controller.capture(PeerCommand.invite) { joinKeyOutcome = $0 }
    }

    private var trimmedPastedKey: String {
        pastedKey.trimmingCharacters(in: .whitespacesAndNewlines)
    }

    /// Opens a Mac's sheet by NAME, which is what the account card knows.
    ///
    /// Matched against the rows this pane has already read. A name that is not
    /// a trusted Mac opens nothing, a sheet built from a name with no row
    /// behind it would have no grants and no leases to draw, and would read as
    /// a Mac with nothing granted.
    private func openSheet(forPeerNamed name: String) {
        sheetPeer = snapshot.rows.first { $0.trust == .trusted && $0.title == name }
    }

    /// Open the sheet `--render-settings` asked for, if it asked for one.
    ///
    /// Scenes 62 and 63 are sheets and a render run cannot click, so the
    /// harness names a scene and this presents it through the pane's own
    /// `.sheet` modifiers: what the PNG shows is then the sheet an operator
    /// gets, presented the way they get it, rather than a picture of a view
    /// hosted somewhere else.
    ///
    /// The Mac is the first trusted row of the pinned fixture rather than a
    /// name written down here: a literal would be a second place that has to
    /// agree with `RenderStates.settingsPaneFixture`, and it would render an
    /// empty sheet the day the fixture's names change.
    private func openRequestedRenderSheet() {
        switch RenderSettings.requestedSheet {
        case .none:
            return
        case .defaults:
            customizing = true
        case .lease:
            // The lease sheet opens ON the Mac sheet, so both are presented:
            // it is a record inside one Mac's record and it has no meaning
            // without the Mac behind it.
            sheetPeer = renderSheetPeer
            editingLease = renderSheetPeer.map { .new(peer: $0.id) }
        case .mac:
            // The Mac with LEASES, when the fixture has one. Scene 63 is
            // about the Lend-from list, and the first trusted row happens to
            // be the one with no lease at all: that PNG showed an empty
            // section and an Add a lease… button, which is a picture of the
            // scene's own subject missing.
            sheetPeer = renderSheetPeer
        }
    }

    /// What the lease sheet calls the Mac: its NAME, looked up from the row
    /// the draft's argv target belongs to. The draft carries the id because
    /// that is what `tcr peer lend` takes, and prose must not.
    /// The handed-key expiry of the lease this draft is editing, from the row
    /// the sheet was opened over. `nil` for a new lease, which has handed
    /// nothing yet, and `nil` for a producer that does not report one.
    private func handedKeyUntil(for draft: LeaseDraft) -> Int64? {
        guard let leaseId = draft.leaseId else { return nil }
        return snapshot.rows
            .first { $0.id == draft.peer }?
            .lend.first { $0.leaseId == leaseId }?
            .handedKeyUntil
    }

    private func leaseSheetLabel(for draft: LeaseDraft) -> String {
        snapshot.rows.first { $0.id == draft.peer }?.title ?? draft.peer
    }

    /// The Mac a render scene opens: the one with LEASES where the fixture has
    /// one, because scene 63 is about the Lend-from list and the first trusted
    /// row happens to be the one with no lease at all.
    private var renderSheetPeer: PeerRowModel? {
        snapshot.rows.first { $0.trust == .trusted && !$0.lend.isEmpty }
            ?? snapshot.rows.first { $0.trust == .trusted }
    }

    /// Put the name `tcr` reports into the field, but only when the field is
    /// EMPTY.
    ///
    /// The field has to show the real name, not a grey prompt, a prompt is
    /// what made it read as a label. But a poll arriving while somebody is
    /// typing must not overwrite what they typed, and a non-empty field is the
    /// one signal available for that: an operator who clears the field gets
    /// the current name back on the next poll, which is the harmless
    /// direction.
    private func fillNameIfEmpty(_ latest: String?) {
        guard editedName.isEmpty, let latest, !latest.isEmpty else { return }
        editedName = latest
    }

    private func commitName() {
        let trimmed = editedName.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty, trimmed != displayName else { return }
        controller.run(["peer", "name", trimmed])
    }

    /// `confirmationDialog(presenting:)` needs a `Bool` binding beside the
    /// value; clearing the value on dismiss is what keeps a cancelled dialog
    /// from leaving the last Mac armed for the next press.
    private var forgettingIsPresented: Binding<Bool> {
        Binding(get: { forgetting != nil }, set: { if !$0 { forgetting = nil } })
    }

    /// The trusted Macs by name, for the Regenerate confirm's own sentence.
    /// "Every Mac that trusted this one" when none is trusted yet, because a
    /// confirm that names nobody must still say what it breaks.
    private var trustedNames: String {
        let names = snapshot.rows.filter { $0.trust == .trusted }.map(\.title)
        return names.isEmpty ? "Every Mac that trusted this one" : PeerFormat.list(names)
    }

    // MARK: - Parts

    /// A grant: the sentence, its detail, and one switch whose argv is the
    /// state it moves to.
    private func grant(
        _ title: String, detail: String, arguments: [String], plaintext: Bool
    ) -> some View {
        VStack(alignment: .leading, spacing: 3) {
            Toggle(isOn: grantBinding(arguments)) {
                Text(title)
                    .foregroundStyle(plaintext ? Tok.unknown : Tok.ink)
            }
            Text(detail)
                .font(.caption)
                .foregroundStyle(Tok.inkFaint)
                .fixedSize(horizontal: false, vertical: true)
        }
    }

    /// A section head: the title on its own line, then ONE sentence with the
    /// badges that used to bracket the title.
    ///
    /// # Two things this pane moved, and what each one cost
    ///
    /// **The badges came off the title**, no "applied live"/"plaintext"
    /// pills in section heads. They are readouts,
    /// and on the title line they bracketed the one word the operator
    /// navigates by. On the sentence line they sit beside the thing they are
    /// about.
    ///
    /// **The per-row captions became this sentence, and it lives in the HEAD,
    /// not in a footer.** Measured, both ways, on the drawn pane: three
    /// top-level captions cost 78 pt; two section footers cost 72 pt and gave
    /// 6 pt back; the same two sentences in the heads cost 28 pt, because a
    /// head is already a laid-out block with its own 35 pt gap above it and a
    /// second line in it is a second line, while a footer is another block
    /// with its own spacing on both sides. That is 44 pt of the 100 this pane
    /// had to find.
    private func peersSectionHead(
        _ title: String, plaintext: Bool = false, _ sentence: String
    ) -> some View {
        VStack(alignment: .leading, spacing: 1) {
            Text(title)
            Text(sentence)
                .font(.caption)
                // The plaintext hue, for the one section that is about another
                // Mac reading a request. The WORD is in the sentence: colour
                // is the second channel, never the meaning.
                .foregroundStyle(plaintext ? Tok.unknown : Tok.inkFaint)
                .textCase(nil)
                .fixedSize(horizontal: false, vertical: true)
        }
    }

    private func peersRow(_ detail: String) -> some View {
        Text(detail)
            .font(.caption)
            .foregroundStyle(Tok.inkFaint)
            .fixedSize(horizontal: false, vertical: true)
    }

    /// The pane's own "what yes does" block, in the hue reserved for plaintext
    /// crossing a machine boundary. Colour never carries the meaning: the
    /// sentence is the meaning and the hue is a second channel.
    private func disclosureNote(_ sentence: String) -> some View {
        Text(sentence)
            .font(.caption)
            .foregroundStyle(Tok.inkDim)
            .fixedSize(horizontal: false, vertical: true)
            .padding(8)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(
                RoundedRectangle(cornerRadius: 8).fill(Tok.unknown.opacity(0.08))
            )
            .overlay(
                RoundedRectangle(cornerRadius: 8)
                    .strokeBorder(Tok.unknown.opacity(0.35), lineWidth: 1)
            )
    }

    private func pillTint(_ role: PeerPill.Role) -> Color {
        switch role {
        case .ok: return Tok.ok
        case .disclosure: return Tok.unknown
        // The same source ``PeerPill/Role`` reads, so a pill on this pane and
        // the identical pill on the Peers tab cannot end up two colours.
        case .info: return Tok.unmeasured
        case .neutral: return Tok.inkFaint
        }
    }

    // MARK: - State
    //
    // The sheets, the confirms, the disclosure, and the two values a sheet is
    // EDITING before Done writes them. Nothing here caches a value `tcr` owns:
    // every readout below comes off the snapshot.

    @State private var advancedOpen = false
    @State private var showingJoinKey = false
    /// `nil` while the invite is in flight. Not an empty string: those are two
    /// different sheets.
    @State private var joinKeyOutcome: PeerCapture?
    @State private var pasting = false
    @State private var pastedKey = ""
    @State private var customizing = false
    @State private var confirmingRegenerate = false
    /// The Mac whose sheet is open, and the Mac whose Forget was pressed. Two
    /// values, because Forget is inside the sheet and dismissing one must not
    /// dismiss the other.
    @State private var sheetPeer: PeerRowModel?
    @State private var forgetting: PeerRowModel?
    /// The Mac whose Block was pressed, while its confirm is up. A third
    /// value, for the same reason Forget has its own: dismissing one confirm
    /// must not disarm another.
    @State private var blocking: PeerRowModel?
    /// The lease being edited or added, while its sheet is open. `nil` is the
    /// sheet being closed, and it is the only state that exists before Save:
    /// nothing is written while this is non-nil.
    @State private var editingLease: LeaseDraft?
    @State private var window: PeerLeaseWindow = .week
    @State private var defaultScope: LendScope = .all
    @State private var editedName = ""
    /// The last answer `tcr peer reach` gave, or `nil` for "not asked yet",
    /// which is what makes the transient state a state. Never persisted: a
    /// mapping outlives neither the router's lifetime nor this window.
    @State private var reachReading: PeerReachReading?

    // MARK: - Values

    private var terms: LeaseTerms { .standard(for: window) }

    private var fractionText: String {
        terms.fraction >= 1 ? "all of it" : String(format: "%.2f", terms.fraction)
    }

    /// Until `tcr peer ls --json` carries them, the readouts this pane cannot
    /// compute are drawn as the honest absence rather than a plausible-looking
    /// value. A pane that invented an id would be showing a person a key to
    /// compare against nothing.
    private var displayName: String {
        snapshot.name ?? Host.current().localizedName ?? "this Mac"
    }
    private var nodeId: String { snapshot.nodeId ?? "not read yet" }
    private var listenAddress: String { snapshot.listenAddress ?? "not read yet" }
    private var via: String { snapshot.via ?? "not read yet" }
    private var hopLimit: String { snapshot.maxHops.map(String.init) ?? "not read yet" }

    /// The groups and account labels the scope picker offers, from the fleet
    /// this window already reads. Sanitized labels only, `tcr status`'s own,
    /// never an email and never a UUID, because they land in argv and in the
    /// peers file and this repository is public.
    private var groupNames: [String] {
        guard case .loaded(let fleet) = dependencies.poller.state else { return [] }
        // `groups` is `nil` from a server that does not report them, which is
        // a different state from "no groups", both offer nothing to pick, so
        // both flatten away here.
        return Array(Set(fleet.accounts.flatMap { $0.groups ?? [] })).sorted()
    }

    private var accountLabels: [String] {
        guard case .loaded(let fleet) = dependencies.poller.state else { return [] }
        return fleet.accounts.map(\.name).sorted()
    }

    // MARK: - Bindings

    /// Every switch on this pane is argv, and its `set` writes the state it
    /// MOVES to. No local `@State`: the value shown is what `tcr` last said,
    /// so a refused write shows as refused.
    /// Whether this Mac is asking to be reachable from off this network.
    /// `nil` on the wire is "not read yet" and draws the shipped default, off.
    private var internetOn: Bool { snapshot.internet ?? false }

    /// The line under the switch, decided in one place
    /// (``PeerInternetReach/state(on:reading:now:)``) from the switch and the
    /// last probe, against this pane's own clock.
    private var internetState: PeerInternetReach {
        PeerInternetReach.state(on: internetOn, reading: reachReading, now: now)
    }

    /// The press: write the setting, then ask the router ONCE.
    ///
    /// The probe is on the press and never on the poll, and that is not a
    /// budget choice. `tcr peer reach --map` writes to the router, and a
    /// repeated request REPLACES an existing mapping's lifetime rather than
    /// adding one (`PeerReachArgs::map`, RFC 6886 s3.3), so a panel probing
    /// every thirty seconds would keep cutting whatever the proxy holds down
    /// to this probe's two minutes. Clearing the reading first is what puts
    /// the line into ``PeerInternetReach/asking`` for the seconds the router
    /// takes, rather than leaving the previous answer on screen under a switch
    /// that has already moved.
    private func pressInternet(_ on: Bool) {
        reachReading = nil
        controller.run(PeerCommand.internet(on: on))
        guard on else { return }
        controller.capture(PeerCommand.reach) { capture in
            switch capture {
            case .text(let output):
                guard
                    let reading = try? PeerReachReading.decode(
                        Data(output.utf8), readAt: Date())
                else {
                    // No silent fallback: an answer this build cannot read is
                    // said as that, in `tcr`'s own bytes, rather than drawn as
                    // a router that stayed quiet.
                    reachReading = PeerReachReading(
                        externalAddress: nil, listenPort: nil,
                        mapping: .refused(output), readAt: Date())
                    return
                }
                reachReading = reading
            case .failed(let message):
                reachReading = PeerReachReading(
                    externalAddress: nil, listenPort: nil, mapping: .refused(message),
                    readAt: Date())
            }
        }
    }

    private var announceNameBinding: Binding<Bool> {
        Binding(
            get: { snapshot.announceName ?? false },
            set: { controller.run(["peer", "find", "--announce-name", $0 ? "on" : "off"]) })
    }

    private var shareBinding: Binding<Bool> {
        Binding(
            get: { snapshot.sharing },
            set: { controller.run(PeerCommand.share(on: $0)) })
    }

    private var discoveryBinding: Binding<Bool> {
        Binding(
            get: { snapshot.finding },
            set: { controller.run(["peer", "discovery", $0 ? "on" : "off"]) })
    }

    private func grantBinding(_ arguments: [String]) -> Binding<Bool> {
        Binding(
            get: { grantIsOn(arguments) },
            set: { controller.run(arguments + [$0 ? "on" : "off"]) })
    }

    /// Which grant a row currently holds, read off the snapshot's own two
    /// booleans. `inspect` has no field on the wire yet, so it reads OFF,
    /// which is the safe direction to be wrong in: the pane shows a grant as
    /// not taken rather than claiming one nobody granted.
    private func grantIsOn(_ arguments: [String]) -> Bool {
        guard arguments.count >= 4 else { return false }
        let peer = arguments[2]
        guard let row = snapshot.rows.first(where: { $0.id == peer }) else { return false }
        switch arguments[3] {
        case "gateway": return row.carries
        case "disclose": return row.serves
        default: return false
        }
    }
}

/// The lease sheet: scene 63's Lend-from row with every one of its numbers as
/// a control, and nothing written until Save.
///
/// # Why a sheet and not more popups on the row
///
/// The row has two popups and an end menu, and between them they could express
/// exactly the three shipped defaults: decision row 12's "30 % of the work
/// group" and "20 % of Fable weekly" were unreachable at any depth, because the
/// only amounts on offer were `LeaseTerms.standard(for:)`. The fraction, the
/// ttl and the in-flight count are three more numbers, and the mockup's own
/// rule 5 says a number needs its unit beside it, which does not fit on a
/// 412 pt row that already carries two popups and a Revoke.
///
/// # It edits a copy
///
/// The draft arrives by value and lives in this view's own state, so Cancel
/// costs nothing and Save is the only writer: one `tcr peer lend` with every
/// flag, which is the same whole-lease write the row's popups do.
struct PeerLeaseSheet: View {
    @State private var draft: LeaseDraft
    /// What the sentences CALL this Mac.
    ///
    /// Separate from `draft.peer`, which is what argv names it: on a trusted
    /// row that is the peer id, and the first render of this sheet read "What
    /// tcr-92hbq5t7yv may spend", which is the identifier an operator has to
    /// look up rather than the name they chose.
    private let peerLabel: String
    /// The clock time behind ``LeaseEnd/until(_:)``, as a `Date` the picker
    /// can drive. Only the hour and minute are ever read out of it.
    @State private var untilTime: Date
    private let groupNames: [String]
    private let accountLabels: [String]
    /// When the key this lease last handed the borrower stops working, as the
    /// LENDER's own record reported it. Carried in rather than derived here:
    /// a draft holds what the operator is choosing, and this is a measurement
    /// about a key already out there.
    private let handedKeyUntil: Int64?
    /// The instant the winding-down line is judged against. The pane's clock,
    /// which is the fixture's pinned one under a render run.
    private let now: Date
    private let onCancel: () -> Void
    private let onSave: (LeaseDraft) -> Void

    init(
        draft: LeaseDraft,
        peerLabel: String? = nil,
        groupNames: [String] = [],
        accountLabels: [String] = [],
        handedKeyUntil: Int64? = nil,
        now: Date = Date(),
        onCancel: @escaping () -> Void = {},
        onSave: @escaping (LeaseDraft) -> Void = { _ in }
    ) {
        _draft = State(initialValue: draft)
        _untilTime = State(initialValue: PeerLeaseSheet.time(from: draft.end))
        self.peerLabel = peerLabel ?? draft.peer
        self.groupNames = groupNames
        self.accountLabels = accountLabels
        self.handedKeyUntil = handedKeyUntil
        self.now = now
        self.onCancel = onCancel
        self.onSave = onSave
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            VStack(alignment: .leading, spacing: 3) {
                Text(draft.isNew ? "Add a lease" : "Edit this lease")
                    .font(.headline)
                Text(
                    "What \(peerLabel) may spend, from this scope alone. Nothing is written "
                        + "until you press Save."
                )
                .font(.caption)
                .foregroundStyle(Tok.inkDim)
                .fixedSize(horizontal: false, vertical: true)
            }

            Form {
                LabeledContent("Lend from") {
                    Menu(draft.scope.label) {
                        PeerScopeMenu(
                            groupNames: groupNames, accountLabels: accountLabels,
                            choose: { draft.scope = $0 })
                    }
                    .fixedSize()
                }
                caption("Only this scope's own accounts serve what it lends.")

                // On the LEASE and not on the Mac: decision row 15 puts
                // `mode` on the grant, so a Mac with two leases can send one
                // of each way.
                LendModeControl(
                    peer: peerLabel,
                    mode: draft.mode,
                    status: PeerLease.handedKeyLine(
                        mode: draft.mode, handedKeyUntil: handedKeyUntil, peer: peerLabel,
                        now: now),
                    onChoose: { draft.mode = $0 })

                Picker("Which allowance", selection: $draft.terms.window) {
                    ForEach(PeerLeaseWindow.allCases, id: \.self) { window in
                        Text(window.label).tag(window)
                    }
                }
                .pickerStyle(.segmented)

                LabeledContent("How much of it") {
                    // A stepper and not a slider: the sheet is 412 pt wide and
                    // a slider there reads to about two per cent, which is
                    // the difference between two grants. Five points a step,
                    // and the number is beside it.
                    Stepper(
                        value: $draft.terms.fraction, in: 0.05...1.0, step: 0.05,
                        label: {
                            Text(fractionLabel)
                                .font(.callout.monospacedDigit())
                                .foregroundStyle(Tok.inkDim)
                        })
                }
                caption(
                    "The fraction of this scope's own headroom in this window that "
                        + "\(peerLabel) may spend for you. The proxy caps it at 0.50.")

                LabeledContent("For how long") {
                    Stepper(
                        value: $draft.terms.ttlSeconds, in: 60...3600, step: 60,
                        label: {
                            Text("\(draft.terms.ttlSeconds) s")
                                .font(.callout.monospacedDigit())
                                .foregroundStyle(Tok.inkDim)
                        })
                }
                caption("The ttl. When it lapses \(peerLabel) asks again.")

                LabeledContent("How many at once") {
                    Stepper(
                        value: $draft.terms.maxInFlight, in: 1...8,
                        label: {
                            Text("\(draft.terms.maxInFlight)")
                                .font(.callout.monospacedDigit())
                                .foregroundStyle(Tok.inkDim)
                        })
                }
                caption(
                    "max in flight. Not a rate limit: how many of your requests one Mac may "
                        + "have open at the same moment.")

                Picker("Ends", selection: endKind) {
                    ForEach(EndKind.allCases, id: \.self) { kind in
                        Text(kind.label).tag(kind)
                    }
                }
                .pickerStyle(.segmented)

                switch draft.end {
                case .none:
                    caption(
                        "No end. The lease runs until you revoke it, renewing every "
                            + "\(draft.terms.ttlSeconds) s.")
                case .after(let span):
                    LabeledContent("For") {
                        Menu(span) {
                            ForEach(PeerLeaseSheet.spans, id: \.self) { choice in
                                Button(choice) { draft.end = .after(choice) }
                            }
                        }
                        .fixedSize()
                    }
                    caption(
                        "Relative, and the lender resolves it to an instant, so the two Macs "
                            + "cannot disagree about which clock ran.")
                case .until:
                    LabeledContent("Until") {
                        DatePicker(
                            "Until", selection: $untilTime, displayedComponents: .hourAndMinute
                        )
                        .labelsHidden()
                        .onChange(of: untilTime) { draft.end = .until(Self.clock(from: $0)) }
                    }
                    caption("A clock time today, which is the spelling tcr peer lend takes.")
                }

                if let refusal = draft.refusal(now: Date()) {
                    // The refusal is shown, and Save is off. No clamp: a sheet
                    // that quietly rounded a zero up, or sent an end already
                    // behind, would write a grant nobody chose.
                    Text(refusal)
                        .font(.caption)
                        .foregroundStyle(Tok.near)
                        .fixedSize(horizontal: false, vertical: true)
                }
            }
            .formStyle(.grouped)

            HStack(spacing: 8) {
                Text(draft.arguments.joined(separator: " "))
                    .font(.caption2.monospaced())
                    .foregroundStyle(Tok.inkFaint)
                    .lineLimit(1)
                    .truncationMode(.middle)
                    .accessibilityLabel("What Save runs")
                Spacer(minLength: 0)
                Button("Cancel", role: .cancel) { onCancel() }
                Button("Save") { onSave(draft) }
                    .keyboardShortcut(.defaultAction)
                    .disabled(draft.refusal(now: Date()) != nil)
            }
        }
        .padding(20)
        .frame(width: 412)
    }

    /// The three ends, as one segmented choice rather than a menu of
    /// hard-coded literals.
    ///
    /// The literal list was `[.none, .after("2h"), .until("18:00")]`, so
    /// "for 30 minutes" and "for 8 hours" were unreachable and "Until 18:00"
    /// went on being offered after 18:00. The kind is chosen here and the
    /// value beside it, and an end already behind is refused rather than sent.
    private enum EndKind: CaseIterable {
        case none, after, until

        var label: String {
            switch self {
            case .none: return "No end"
            case .after: return "For a while"
            case .until: return "Until a time"
            }
        }
    }

    private var endKind: Binding<EndKind> {
        Binding(
            get: {
                switch draft.end {
                case .none: return .none
                case .after: return .after
                case .until: return .until
                }
            },
            set: { kind in
                switch kind {
                case .none: draft.end = .none
                case .after: draft.end = .after(PeerLeaseSheet.spans[1])
                case .until: draft.end = .until(Self.clock(from: untilTime))
                }
            })
    }

    /// The spans `--for` takes. Four, bracketing the day: half an hour, the
    /// mockup's own two hours, a working day and a whole one.
    private static let spans = ["30m", "2h", "8h", "24h"]

    private var fractionLabel: String {
        draft.terms.fraction >= 1
            ? "all of it" : String(format: "%.2f", draft.terms.fraction)
    }

    private static func clock(from date: Date, calendar: Calendar = .current) -> String {
        let parts = calendar.dateComponents([.hour, .minute], from: date)
        return String(format: "%02d:%02d", parts.hour ?? 0, parts.minute ?? 0)
    }

    /// The picker's starting instant: the draft's own clock time when it has
    /// one, and otherwise now. Never a fabricated hour.
    private static func time(from end: LeaseEnd, now: Date = Date()) -> Date {
        guard case .until(let clock) = end else { return now }
        let parts = clock.split(separator: ":")
        guard parts.count == 2, let hour = Int(parts[0]), let minute = Int(parts[1]),
            let date = Calendar.current.date(
                bySettingHour: hour, minute: minute, second: 0, of: now)
        else { return now }
        return date
    }

    private func caption(_ sentence: String) -> some View {
        Text(sentence)
            .font(.caption)
            .foregroundStyle(Tok.inkFaint)
            .fixedSize(horizontal: false, vertical: true)
    }
}

/// The scope menu's items: `All accounts`, every group, every account label.
///
/// Its own view rather than a method on the pane, because two sheets and a row
/// popup offer it and a second copy is a picker that can offer a group the
/// other one does not.
struct PeerScopeMenu: View {
    let groupNames: [String]
    let accountLabels: [String]
    let choose: (LendScope) -> Void

    var body: some View {
        Button("All accounts") { choose(.all) }
        if !groupNames.isEmpty {
            Divider()
            ForEach(groupNames, id: \.self) { name in
                Button("Group: \(name)") { choose(.group(name)) }
            }
        }
        if !accountLabels.isEmpty {
            Divider()
            ForEach(accountLabels, id: \.self) { label in
                Button("Account: \(label)") { choose(.accounts([label])) }
            }
        }
    }
}

private struct PeersTag: View {
    let timing: SettingsRowTiming

    private var tint: Color {
        switch timing {
        case .live: return Tok.ok
        case .boot: return Tok.near
        case .readOnly: return Tok.inkFaint
        case .nextLaunch: return Tok.near
        }
    }

    var body: some View {
        Text(timing.label)
            .font(.caption2.weight(.semibold))
            .foregroundStyle(tint)
            .padding(.horizontal, 6)
            .padding(.vertical, 2)
            .overlay(
                RoundedRectangle(cornerRadius: 6).strokeBorder(Tok.line(tint), lineWidth: 1)
            )
    }
}
