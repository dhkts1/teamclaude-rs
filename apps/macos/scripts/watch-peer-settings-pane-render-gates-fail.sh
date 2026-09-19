#!/opt/homebrew/bin/bash
# Watch each settings-pane render gate fail, one mutation at a time.
#
# Same contract as `watch-peer-panel-view-gates-fail.sh`, which this is a copy of
# in shape and not in content: break the production line the gate exists to
# catch, run ONLY that gate, require a failure that is the ASSERTION going off,
# restore the bytes from a copy taken before the edit, require a pass again.
# Restore is a byte copy in a trap and never `git checkout`, this worktree
# holds other uncommitted work.
#
# Usage: apps/macos/scripts/watch-peer-settings-pane-render-gates-fail.sh
# Exit 0 only if every mutation went red and every restore went green.
set -uo pipefail

pkg="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
backup="$(mktemp -d "${TMPDIR:-/tmp}/peer-wave5-gates.XXXXXX")"
failures=0

restore_all() {
    for saved in "$backup"/*.bak; do
        [[ -e "$saved" ]] || continue
        target="$(cat "$saved.path")"
        cp "$saved" "$target"
    done
}
trap 'restore_all; rm -rf "$backup"' EXIT

save() {
    local file="$1" slug
    slug="$(echo "$file" | tr '/' '_')"
    cp "$pkg/$file" "$backup/$slug.bak"
    printf '%s' "$pkg/$file" >"$backup/$slug.bak.path"
}

# mutate <file> <from> <to>, applied with python so a multi-line Swift
# literal is replaced exactly, not by a regex.
mutate() {
    local file="$1" from="$2" to="$3"
    save "$file"
    FROM="$from" TO="$to" TARGET="$pkg/$file" python3 - <<'PY'
import os
target, frm, to = os.environ["TARGET"], os.environ["FROM"], os.environ["TO"]
text = open(target).read()
if frm not in text:
    raise SystemExit(f"ANCHOR MISSING in {target}: {frm[:60]!r}")
open(target, "w").write(text.replace(frm, to, 1))
PY
}

# expect_red <label> <test-filter>
expect_red() {
    local label="$1" filter="$2" log
    log="$backup/$(echo "$filter" | tr '/' '_').log"
    if swift test --package-path "$pkg" --filter "$filter" >"$log" 2>&1; then
        echo "NOT A GATE: $label passed with the production line broken ($filter)"
        failures=$((failures + 1))
    elif grep -qE "error: -\[|Fatal error|Crashed|signal" "$log"; then
        echo "red as expected: $label"
    else
        echo "NOT A RED: $label failed for a reason that is not its assertion ($filter)"
        grep -m3 -E "error|warning: " "$log" || tail -3 "$log"
        failures=$((failures + 1))
    fi
    restore_all
    if swift test --package-path "$pkg" --filter "$filter" >"$log" 2>&1; then
        echo "  green again after restore: $label"
    else
        echo "RESTORE FAILED: $label is still red with the bytes put back ($filter)"
        tail -5 "$log"
        failures=$((failures + 1))
    fi
}

echo "== 1. a zero offset throws the clip view's resting origin away"
mutate "Sources/TcrBarCore/RenderScrollTarget.swift" \
    '    public static func clipOrigin(restingY: CGFloat, offset: CGFloat) -> CGFloat {
        restingY + offset
    }' \
    '    public static func clipOrigin(restingY: CGFloat, offset: CGFloat) -> CGFloat {
        offset
    }'
expect_red "a pane that fits stays where it rests" \
    "RenderScrollTargetTests/testAZeroOffsetLeavesThePaneAtItsRestingOrigin"

echo "== 2. the harness hands AppKit a bare offset again"
mutate "Sources/TcrBar/RenderSettings.swift" \
    'RenderScrollTarget.clipOrigin(restingY: restingY, offset: offset)' \
    'offset'
expect_red "the harness scrolls from the resting origin" \
    "PeersSettingsPaneRenderWiringTests/testTheHarnessScrollsFromTheRestingOrigin"

echo "== 3. the viewport goes back to the clip view's bounds"
mutate "Sources/TcrBar/RenderSettings.swift" \
    'let viewportHeight = scroll.contentView.frame.height + restingY' \
    'let viewportHeight = scroll.contentView.bounds.height'
expect_red "the viewport is the visible height" \
    "PeersSettingsPaneRenderWiringTests/testTheViewportIsTheVisibleHeightNotTheClipBounds"

echo "== 4. the Peers capture grows its own window again"
mutate "Sources/TcrBar/RenderSettings.swift" \
    '        return SettingsWindowController.shippedContentSize' \
    '        return NSSize(width: 660, height: 1200)'
expect_red "the capture uses the shipped window" \
    "PeersSettingsPaneRenderWiringTests/testThePeersCaptureUsesTheShippedWindowSize"

echo "== 5. one row grows past the budget and the pane stops fitting"
# The mutation someone could actually make by accident: put a sentence back on a row
# by charging its detail height. Two of them is the pane over the window.
mutate "Sources/TcrBarCore/PeerPaneLayout.swift" \
    '            explainedRows: 0, sectionSentences: 2)' \
    '            explainedRows: 2, sectionSentences: 2)'
expect_red "the drawn pane fits what the window shows" \
    "PeerPaneLayoutTests/testTheDrawnPaneFitsWhatTheWindowActuallyShows"

echo "== 6. a metric is tuned instead of measured"
mutate "Sources/TcrBarCore/PeerPaneLayout.swift" \
    '        peerRowHeight: 37, groupChrome: 0, groupGap: 35, disclosureHeight: 44,' \
    '        peerRowHeight: 20, groupChrome: 0, groupGap: 20, disclosureHeight: 44,'
expect_red "the arithmetic still reproduces the drawn pane" \
    "PeerPaneLayoutTests/testTheArithmeticReproducesTheDrawnPaneHeight"

echo "== 7. the pane keeps its own copy of the metric figures"
mutate "Sources/TcrBar/PeersSettingsView.swift" \
    '    static let shippedMetrics = PeerPaneLayout.drawnMetrics' \
    '    static let shippedMetrics = PeerPaneLayout.Metrics(
        sectionHeadHeight: 21, rowHeight: 37, rowDetailHeight: 26, knockRowHeight: 40,
        peerRowHeight: 37, groupChrome: 0, groupGap: 35, disclosureHeight: 44,
        subLineHeight: 14, sectionSentenceHeight: 14, outerMargins: 46, unheadedGap: 10)'
expect_red "no second copy of the drawn metrics" \
    "PeersSettingsPaneRenderWiringTests/testThePaneHoldsNoSecondCopyOfTheDrawnMetrics"

echo "== 8. a badge goes back into a section head"
mutate "Sources/TcrBar/PeersSettingsView.swift" \
    '        VStack(alignment: .leading, spacing: 1) {
            Text(title)
            Text(sentence)' \
    '        VStack(alignment: .leading, spacing: 1) {
            Text(title)
            PeersTag(timing: .live)
            Text(sentence)'
expect_red "no badge pills in the section heads" \
    "PeersSettingsPaneRenderWiringTests/testNoBadgePillsInTheSectionHeads"

echo "== 9. the Defaults readout may wrap again"
mutate "Sources/TcrBar/PeersSettingsView.swift" \
    '                        .lineLimit(1)
                        .truncationMode(.tail)' \
    '                        .truncationMode(.tail)'
expect_red "the Defaults row is one line" \
    "PeersSettingsPaneRenderWiringTests/testTheDefaultsRowIsOneLine"

echo "== 10. the long readout comes back and the row wraps"
mutate "Sources/TcrBarCore/PeerLease.swift" \
    '            return "\(window.shortLabel) \(amount)"' \
    '            return "\(window.label) \(amount)"'
expect_red "the Defaults readout fits one line" \
    "PeerLeaseTests/testTheDefaultsReadoutFitsOneLine"

echo "== 11. an allowance disappears from the readout"
mutate "Sources/TcrBarCore/PeerLease.swift" \
    '        let per = PeerLeaseWindow.allCases.map { window -> String in' \
    '        let per = PeerLeaseWindow.allCases.dropFirst().map { window -> String in'
expect_red "the readout names every allowance" \
    "PeerLeaseTests/testTheDefaultsReadoutNamesEveryAllowanceAndItsAmount"

echo "== 12. the name field loses its border and reads as a label"
mutate "Sources/TcrBar/PeersSettingsView.swift" \
    '                    .textFieldStyle(.roundedBorder)' \
    '                    .labelsHidden()'
expect_red "the name is an editable field" \
    "PeersSettingsPaneRenderWiringTests/testTheNameIsAnEditableFieldThatCommitsOnReturn"

echo "== 13. a poll overwrites what the operator is typing"
mutate "Sources/TcrBar/PeersSettingsView.swift" \
    '        guard editedName.isEmpty, let latest, !latest.isEmpty else { return }' \
    '        guard let latest, !latest.isEmpty else { return }'
expect_red "the fill never lands on a typed field" \
    "PeersSettingsPaneRenderWiringTests/testTheFieldIsFilledFromTheSnapshotButNeverOverTyping"

echo "== 14. the id is a row behind Advanced as well as a sub-line"
mutate "Sources/TcrBar/PeersSettingsView.swift" \
    '                LabeledContent("Listening on") {' \
    '                LabeledContent("Id") { Text(nodeId) }
                LabeledContent("Listening on") {'
expect_red "the id is in one place" \
    "PeersSettingsPaneRenderWiringTests/testTheIdIsTheNameRowsSubLineAndNowhereElse"

echo "== 15. the pairing sentence leaves the accessibility tree too"
mutate "Sources/TcrBar/PeersSettingsView.swift" \
    '        .accessibilityHint(PeerAdmission.knockDetail)' \
    '        .accessibilityHint("")'
expect_red "the knock row keeps its sentence for screen readers" \
    "PeersSettingsPaneRenderWiringTests/testTheKnockRowKeepsItsSentenceForScreenReadersOnly"

echo
if [[ $failures -eq 0 ]]; then
    echo "all 15 gates went red on their own mutation and green again after restore"
else
    echo "$failures gate(s) did not behave as a gate"
fi
exit "$failures"
