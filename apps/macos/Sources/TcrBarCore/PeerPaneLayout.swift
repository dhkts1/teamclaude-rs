import CoreGraphics

/// The SHORT Settings > Peers pane's height budget, as arithmetic a test can
/// run.
///
/// Gil on the first pane: "way too long, almost unusable". The short pane
/// (`mockups/settings-peers-short.html`, scenes 59 to 61) is the design, and
/// its whole claim is a number: **two trusted Macs plus one pairing request fit
/// the 460×500 hole without scrolling**, 493.39 px measured against the 500
/// available, and 434.95 px with no request waiting. That claim is what this
/// type holds, so it is checked by something other than a person looking at a
/// PNG.
///
/// # Why the figures are parameters and the BUDGET is not
///
/// Same split as ``PeerPanelHeight``: `Tok` and the pane's own fonts live in
/// the executable target, so a test that restated a row height would be
/// testing its own arithmetic. The caller passes what it draws. The one value
/// that is NOT a parameter is ``availableHeight``, an assertion about that
/// number is an assertion about the shipped window, which is the thing the
/// mockup measured against.
///
/// # Two metric sets, because a browser and AppKit charge differently
///
/// The mockup's 493.39 px is its own HTML layout of the design, and this type
/// reproduces THAT number from the mockup's CSS parts, `PeerPaneLayoutTests`
/// is written against it and it is a cross-check of the ARITHMETIC, not of the
/// app.
///
/// A grouped `Form` charges more, and the first measured pane's figures said so: the drawn
/// pane was **675 pt** at two Macs and one request, against the same
/// arithmetic's 492. Every explained row was a `min-height` row PLUS a caption
/// row of its own; the `Form` also charged outer padding and a 35 pt gap
/// between groups that no CSS line mentions. So there is a second set of
/// metrics, ``PeersSettingsPane/shippedMetrics``, every figure read off the
/// rendered capture, and the same arithmetic run at those figures predicts
/// the drawn pane inside a point.
///
/// The shrink then spent the difference: the id became a sub-line, three
/// captions became two head sentences, the knock sentence went to the tab that
/// already draws it, the Defaults readout went to one line, and the badges came
/// off the heads. Measured after, by the same command:
///
///     apps/macos/.build/debug/TcrBar --render-settings /tmp/peers
///     # peers: scrolled to 0 pt to show the last 44 pt of the pane
///     #        (document 538 pt, viewport 540 pt, resting -52 pt)
///
/// **538 against the 540 pt an operator sees.** Nothing scrolls, and the
/// margin is two points, which is the honest figure and not a comfortable
/// one: the structural floor for three cards, three heads, seven rows and a
/// disclosure in a grouped `Form` is about 535, so a font metric or one more
/// sentence puts this pane back into a scroll. The two levers are unchanged
/// and both are the owner's call: a taller Settings window, or two sections
/// instead of three.
///
/// # What "fits" means here
///
/// The pane is a `Form`, so it scrolls when it overflows and nothing is lost:
/// the failure this guards is subtler than a clipped control. The Advanced
/// disclosure is the LAST row, and an operator who has to scroll to reach it is
/// an operator who does not know it is there; that is the reachable-without-a-
/// gesture property scene 59 argues for, and the reason the disclosure is
/// closed by default (open, the pane is 865 px and scrolling is the trade the
/// operator who went looking pays).
public enum PeerPaneLayout {
    /// The hole the DESIGN draws into: 500 pt, the figure the mockup measured
    /// every scene against, the Settings window's 581 pt frame less its title
    /// bar and the form's own outer margins.
    ///
    /// It is the right target for the mockup's metrics, whose figures are of
    /// the section boxes alone. For the drawn pane, whose model includes the
    /// `Form`'s own margins, the number to compare against is
    /// ``visibleDocumentHeight``.
    public static let availableHeight: CGFloat = 500

    /// How much of the pane's DOCUMENT an operator sees in the shipped
    /// Settings window without touching the wheel: 540 pt.
    ///
    /// Measured 2026-09-18, not derived: `SettingsWindowController` asks for
    /// 540 pt of content and SwiftUI's own split-view minimum makes the window
    /// 592, of which the toolbar takes a 52 pt inset off the top:
    /// `--render-settings` prints all three (`viewport 540 pt, resting -52
    /// pt`). So `document ≤ 540` is the no-scroll condition, and it is the one
    /// the gate uses for the drawn pane.
    public static let visibleDocumentHeight: CGFloat = 540

    /// What a grouped `Form` charges the drawn pane, in points, MEASURED off
    /// the rendered capture, not read off the mockup's CSS.
    ///
    /// # How to re-derive every figure
    ///
    ///     apps/macos/.build/debug/TcrBar --render-settings /tmp/peers
    ///     # peers: … (document 538 pt, viewport 540 pt, resting -52 pt,
    ///     #           pane estimate 537 pt)
    ///
    /// The total is printed. The PARTS come off the capture by scanning a
    /// column for the group cards' own background, which is how each of these
    /// twelve numbers was read (2 px per point, dark appearance):
    ///
    ///     python3 - "$png" <<'EOF'
    ///     from PIL import Image
    ///     im = Image.open(__import__("sys").argv[1]).convert("RGB")
    ///     card = lambda y: sum(
    ///         all(abs(a - b) < 4 for a, b in zip(im.getpixel((x, y)), (44, 47, 49)))
    ///         for x in range(470, 1240, 7)) > 60
    ///     state, start = None, 0
    ///     for y in range(im.size[1]):
    ///         if card(y) != state:
    ///             if state is not None:
    ///                 print(f"{'card' if state else 'gap '} {start/2:6.1f} .. {y/2:6.1f}")
    ///             state, start = card(y), y
    ///     EOF
    ///
    /// A figure changed by hand rather than by that command is a guess with a
    /// test's authority attached.
    public static let drawnMetrics = Metrics(
        sectionHeadHeight: 21, rowHeight: 37, rowDetailHeight: 26, knockRowHeight: 40,
        peerRowHeight: 37, groupChrome: 0, groupGap: 35, disclosureHeight: 44,
        subLineHeight: 14, sectionSentenceHeight: 14, outerMargins: 46, unheadedGap: 10)

    /// What the drawn pane draws: no top-level row carries its own sentence,
    /// and two section heads carry one.
    public static func drawnContent(
        trustedMacs: Int, pendingKnocks: Int, advancedOpen: Bool = false,
        advancedRows: Int = 7
    ) -> Content {
        Content(
            trustedMacs: trustedMacs, pendingKnocks: pendingKnocks,
            advancedOpen: advancedOpen, advancedRows: advancedRows,
            explainedRows: 0, sectionSentences: 2)
    }

    /// What each part of the short pane charges. Every value is an input.
    public struct Metrics: Equatable, Sendable {
        /// A section head and the gap above its group.
        public let sectionHeadHeight: CGFloat
        /// A plain row: one label line and its control.
        public let rowHeight: CGFloat
        /// The extra a row's explaining sentence adds, when it has one.
        public let rowDetailHeight: CGFloat
        /// A pairing-request row: a bold title, its sentence, and two
        /// controls. Taller than a plain row because it is the one row on the
        /// pane with a decision on it.
        public let knockRowHeight: CGFloat
        /// One compact trusted-Mac row. 40 pt in the mockup, and the reason
        /// the three per-Mac switches moved into a sheet.
        public let peerRowHeight: CGFloat
        /// A group's own edges: its border, and the padding inside it.
        public let groupChrome: CGFloat
        /// The gap between two groups.
        public let groupGap: CGFloat
        /// The closed Advanced disclosure row.
        public let disclosureHeight: CGFloat
        /// A second line INSIDE a row's label, this Mac's id under its name.
        /// Zero for a layout that has no sub-line.
        public let subLineHeight: CGFloat
        /// The second line of a section HEAD: one sentence, which is where
        /// the section heads carry the explanations that used to be per-row
        /// captions.
        /// Zero for the mockup, which nests its sentences in the rows.
        ///
        /// 14 pt on the drawn pane against 26 for a caption row and 36 for a
        /// section footer, all three measured, a head is already a laid-out
        /// block and a second line in it is a second line.
        public let sectionSentenceHeight: CGFloat
        /// The gap above a group with NO head of its own: the Advanced
        /// disclosure. 10 pt drawn, against 35 for a group under a head.
        public let unheadedGap: CGFloat
        /// Everything OUTSIDE the four groups: the `Form`'s own padding above
        /// the first section head and below the last group.
        ///
        /// Zero for the mockup, whose measured figures are of the sections
        /// alone. Not zero for a drawn `Form`, and the reason the arithmetic
        /// used to read 180 pt under the pane it was modelling.
        public let outerMargins: CGFloat

        public init(
            sectionHeadHeight: CGFloat, rowHeight: CGFloat, rowDetailHeight: CGFloat,
            knockRowHeight: CGFloat, peerRowHeight: CGFloat, groupChrome: CGFloat,
            groupGap: CGFloat, disclosureHeight: CGFloat, subLineHeight: CGFloat = 0,
            sectionSentenceHeight: CGFloat = 0, outerMargins: CGFloat = 0,
            unheadedGap: CGFloat? = nil
        ) {
            self.sectionHeadHeight = sectionHeadHeight
            self.rowHeight = rowHeight
            self.rowDetailHeight = rowDetailHeight
            self.knockRowHeight = knockRowHeight
            self.peerRowHeight = peerRowHeight
            self.groupChrome = groupChrome
            self.groupGap = groupGap
            self.disclosureHeight = disclosureHeight
            self.subLineHeight = subLineHeight
            self.sectionSentenceHeight = sectionSentenceHeight
            self.outerMargins = outerMargins
            // The mockup gives the Advanced group the same 5 px margin as
            // every other group, so an omitted value is the group gap, the
            // drawn pane is the one that charges two different numbers.
            self.unheadedGap = unheadedGap ?? groupGap
        }
    }

    /// What the pane is being asked to draw. Three sections and a disclosure,
    /// and the only two things that VARY are how many Macs are trusted and how
    /// many are knocking, which is exactly what the mockup's two measurements
    /// differ by.
    public struct Content: Equatable, Sendable {
        public let trustedMacs: Int
        public let pendingKnocks: Int
        /// Whether the Advanced disclosure is open. Open, the pane overflows
        /// on purpose (865 px in scene 61) and this type says so rather than
        /// pretending it fits.
        public let advancedOpen: Bool
        /// How many rows the open Advanced disclosure adds. A parameter
        /// because the seven rows are the pane's, not this type's.
        public let advancedRows: Int

        /// How many TOP-LEVEL rows carry their own explaining sentence.
        ///
        /// Three in the mockup (Name, Announce my name, and the Share switch)
        /// and that is the default, because the mockup's two measured totals
        /// are what this type is cross-checked against. The shipped pane has
        /// none: the sentences became ``sectionFooters``.
        public let explainedRows: Int
        /// How many section HEADS carry a sentence instead.
        public let sectionSentences: Int

        public init(
            trustedMacs: Int, pendingKnocks: Int, advancedOpen: Bool = false,
            advancedRows: Int = 7, explainedRows: Int = 3, sectionSentences: Int = 0
        ) {
            self.trustedMacs = trustedMacs
            self.pendingKnocks = pendingKnocks
            self.advancedOpen = advancedOpen
            self.advancedRows = advancedRows
            self.explainedRows = explainedRows
            self.sectionSentences = sectionSentences
        }
    }

    /// The pane's drawn height.
    ///
    /// Section by section, in pane order, so a reader can check it against the
    /// mockup:
    ///
    /// 1. **This Mac**, Name, Announce my name. Two rows, both with a
    ///    sentence.
    /// 2. **Sharing**, the Share switch (with a sentence) and the Defaults
    ///    row with `Customize…` (one line of numbers, no sentence).
    /// 3. **Trusted Macs**, one knock row per request, then one compact row
    ///    per Mac.
    /// 4. The closed **Advanced** disclosure, or its rows when it is open.
    public static func height(_ content: Content, metrics m: Metrics) -> CGFloat {
        let thisMac =
            m.sectionHeadHeight + m.groupChrome
            + 2 * m.rowHeight
            + m.subLineHeight  // this Mac's id, under its name
        let sharing =
            m.sectionHeadHeight + m.groupChrome
            + 2 * m.rowHeight  // the Share switch, and Defaults on one line
        let trusted =
            m.sectionHeadHeight + m.groupChrome
            + CGFloat(content.pendingKnocks) * m.knockRowHeight
            + CGFloat(content.trustedMacs) * m.peerRowHeight
        let advanced =
            m.groupChrome + m.disclosureHeight
            + (content.advancedOpen
                ? CGFloat(content.advancedRows) * (m.rowHeight + m.rowDetailHeight) : 0)
        // The sentences, wherever this pane puts them: inside the rows that
        // have one, or in the second line of a section head.
        let sentences =
            CGFloat(content.explainedRows) * m.rowDetailHeight
            + CGFloat(content.sectionSentences) * m.sectionSentenceHeight
        // Three gaps between the four groups, and the last one is smaller:
        // the Advanced group has no head above it.
        return thisMac + sharing + trusted + advanced + sentences
            + 2 * m.groupGap + m.unheadedGap + m.outerMargins
    }

    /// Whether the pane fits its hole without scrolling.
    public static func fits(_ content: Content, metrics: Metrics) -> Bool {
        height(content, metrics: metrics) <= availableHeight
    }

    /// How far past the hole the pane runs, or zero when it fits. The number
    /// scene 61 quotes (365 px) for the open disclosure.
    public static func overflow(_ content: Content, metrics: Metrics) -> CGFloat {
        max(0, height(content, metrics: metrics) - availableHeight)
    }

    /// What the pane this one REPLACES would have measured, at the same
    /// metrics.
    ///
    /// Gil on it: "way too long, almost unusable". It is here as a
    /// measurement rather than as a memory, because "the short pane is
    /// shorter" is the claim the redesign rests on and the only honest way to
    /// check it is to run both through one set of numbers. Nothing draws this;
    /// the only caller is the gate.
    ///
    /// Its shape, from the retired pane (`mockups/settings-peers.html` scene
    /// 58, as `PeersSettingsPane` built it): an intro paragraph; **This Mac**
    /// with seven explained rows (name, announce, id, listening, join key,
    /// paste, regenerate); **Sharing defaults** with a segmented control and
    /// five explained rows plus a disclosure note; **Trusted Macs** with
    /// THREE explained grant switches per Mac plus its own row, and a closing
    /// note; **Advanced** with three explained rows and a note, every one of
    /// them at the top level, with no sheet and no disclosure anywhere.
    public static func retiredLongPaneHeight(trustedMacs: Int, metrics m: Metrics) -> CGFloat {
        let explained = m.rowHeight + m.rowDetailHeight
        let intro = m.groupChrome + explained
        let thisMac = m.sectionHeadHeight + m.groupChrome + 7 * explained
        let sharing = m.sectionHeadHeight + m.groupChrome + 6 * explained + m.rowDetailHeight
        let trusted =
            m.sectionHeadHeight + m.groupChrome
            + CGFloat(trustedMacs) * (m.rowHeight + 3 * explained) + m.rowDetailHeight
        let advanced = m.sectionHeadHeight + m.groupChrome + 3 * explained + m.rowDetailHeight
        return intro + thisMac + sharing + trusted + advanced + 4 * m.groupGap
    }
}
