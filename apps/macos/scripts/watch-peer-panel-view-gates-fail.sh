#!/opt/homebrew/bin/bash
# Watch each panel-view gate fail, one mutation at a time.
#
# Every gate this script covers is a claim about production behaviour, and a test
# that has only ever passed proves nothing about what it guards (`CLAUDE.md`,
# "Verifying a change"). So: break the production line the gate exists to
# catch, run ONLY that gate, require a failure, restore the bytes, and require
# a pass again.
#
# Restore is a byte copy of a file taken before the edit and put back in a
# trap, never `git checkout`, which would also wipe every other uncommitted
# change in this worktree.
#
# Usage: apps/macos/scripts/watch-peer-panel-view-gates-fail.sh
# Exit 0 only if every mutation went red and every restore went green.
set -uo pipefail

pkg="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
backup="$(mktemp -d "${TMPDIR:-/tmp}/peer-wave4-gates.XXXXXX")"
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

# mutate <file> <python-replacement-script-inline>, applied with python so a
# multi-line Swift literal is replaced exactly, not by a regex.
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
        # A red has to be the ASSERTION going off, or the process dying the way
        # the bug kills it. A red that is really a chdir or a compile error is
        # a broken harness reporting a verdict about nothing, which is how the
        # first run of this script "proved" ten gates.
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
        echo "  last lines:"
        tail -5 "$log"
        failures=$((failures + 1))
    fi
}

echo "== 1. TcrTool writes with the API that raises on a dead pipe"
mutate "Sources/TcrBarCore/TcrTool.swift" \
    'if stdin != nil { ignoreSIGPIPE() }' \
    'if stdin != nil { signal(SIGPIPE, SIG_DFL) }'
expect_red "a write to an exited child kills this process" \
    "TcrToolStdinTests/testWritingToAChildThatAlreadyExitedDoesNotKillThisProcess"

echo "== 2. an unknown lease scope widens to All instead of refusing"
mutate "Sources/TcrBarCore/PeerLease.swift" \
    '        return nil
    }
}

extension String {' \
    '        return .all
    }
}

extension String {'
expect_red "an unknown scope must not become all" \
    "PeerLeaseTests/testAnUnknownScopeRefusesRatherThanWidening"

echo "== 3. the account card lists every Mac instead of two and a count"
mutate "Sources/TcrBarCore/PeerLease.swift" \
    'let shown = entries.prefix(2).map' \
    'let shown = entries.prefix(9).map'
expect_red "a third Mac becomes 'and 1 more'" \
    "PeerLeaseTests/testAThirdMacBecomesAndOneMore"

echo "== 4. Advanced prints its own cap literal instead of the binary's"
mutate "Sources/TcrBarCore/PeerAdmission.swift" \
    '            ("Macs shown at once", "\(foundRows)"),' \
    '            ("Macs shown at once", "12"),'
expect_red "the caps rows come from the caps object" \
    "PeerAdmissionTests/testChangedCapsChangeTheRowsWithNoEditToThePanel"

echo "== 5. the panel recomputes a count the producer already sent"
mutate "Sources/TcrBarCore/PeerListDocument.swift" \
    '        self.pendingCount =
            try c.decodeIfPresent(Int.self, forKey: .pendingCount) ?? self.pending.count' \
    '        self.pendingCount = self.pending.count'
expect_red "the producer's count wins over the row count" \
    "PeerListDocumentTests/testACountTheProducerSentBeatsTheRowCount"

echo "== 6. Unblock is handed the instance id instead of the address"
mutate "Sources/TcrBarCore/PeerAdmission.swift" \
    'public static func unblock(address: String) -> [String] { ["peer", "unblock", address] }' \
    'public static func unblock(address: String) -> [String] { ["peer", "unblock", "instance"] }'
expect_red "unblock takes the address" \
    "PeerAdmissionTests/testUnblockTakesTheAddressAndNotTheInstanceId"

echo "== 7. the join link is passed as an argument"
mutate "Sources/TcrBarCore/PeerJoinLink.swift" \
    '                arguments: ["peer", "join", "--stdin"], stdin: url.absoluteString))' \
    '                arguments: ["peer", "join", url.absoluteString], stdin: url.absoluteString))'
expect_red "the link never reaches argv" \
    "PeerJoinLinkTests/testTheSecretIsNeverInArgv"

echo "== 8. the pane's height model loses a whole section"
# The return line gained the shrink's own terms (the head sentences, the
# Advanced group's smaller gap, the Form's outer margins), so the anchor moved
# with it; the mutation is the same one, lose a whole section.
mutate "Sources/TcrBarCore/PeerPaneLayout.swift" \
    '        return thisMac + sharing + trusted + advanced + sentences' \
    '        return thisMac + trusted + advanced + sentences'
expect_red "the solved sentence height stays inside what the CSS can produce" \
    "PeerPaneLayoutTests/testTheWrappedSentenceHeightSolvedFromTheMockupIsPlausible"

echo "== 9. the render harness goes back to aiming at a constant"
# Aimed at the whole function, not at one of its two early exits: a pane that
# FITS returns zero through the reachable guard first, so mutating the
# in-viewport check alone leaves the gate green and proves nothing (measured on
# this script's second run).
mutate "Sources/TcrBarCore/RenderScrollTarget.swift" \
    '        let reachable = max(0, documentHeight - viewportHeight)' \
    '        if true { return 470 }
        let reachable = max(0, documentHeight - viewportHeight)'
expect_red "a pane that fits is never scrolled into the bounce region" \
    "RenderScrollTargetTests/testAPaneThatFitsIsNeverScrolledIntoTheBounceRegion"

echo "== 10. the limited footer is drawn even when nothing was held back"
mutate "Sources/TcrBarCore/PeerAdmission.swift" \
    '        guard limited > 0 else { return nil }' \
    '        guard limited >= 0 else { return nil }'
expect_red "no footer when nothing was held back" \
    "PeerAdmissionTests/testTheFooterIsAbsentWhenNothingWasHeldBack"

# ---------------------------------------------------------------------------
# Batch two: the WIRING gates, which are source-reading rather than value
# tests. Each mutation is one that could be made by accident, and
# every one still compiles, a mutation that only breaks the build proves the
# compiler works, not that the gate does.
# ---------------------------------------------------------------------------

echo "== 11. the harness aims at a constant instead of asking the pane"
mutate "Sources/TcrBar/RenderSettings.swift" \
    'tab == .peers ? PeersSettingsPane.lastRowHeight : nil' \
    'tab == .peers ? 470 : nil'
expect_red "the harness takes its scroll target from the pane" \
    "PeersPanelViewWiringTests/testTheHarnessTakesItsScrollTargetFromThePane"

echo "== 12. Unblock is given an address that is not the row's"
mutate "Sources/TcrBar/PeersSettingsView.swift" \
    'controller.run(PeerCommand.unblock(address: ban.addr))' \
    'controller.run(PeerCommand.unblock(address: "0.0.0.0"))'
expect_red "the Blocked list unblocks the row it is drawn from" \
    "PeersPanelViewWiringTests/testTheBlockedListShowsBothStatesAndUnblocksByAddress"

echo "== 13. the limited footer is computed against a hard zero"
mutate "Sources/TcrBar/PanelV4/PeersTabV4.swift" \
    'PeerAdmission.limitedFooter(shown: rows.count, limited: limited)' \
    'PeerAdmission.limitedFooter(shown: rows.count, limited: 0)'
expect_red "the footer reads the limited count tcr reported" \
    "PeersPanelViewWiringTests/testTheTabDrawsTheLimitedFooterFromTheSnapshot"

echo "== 14. Accept is handed the knocking address instead of the instance id"
mutate "Sources/TcrBar/PanelV4/PeersTabV4.swift" \
    '                "Accept", PeerCommand.accept(instance: knock.instanceId),' \
    '                "Accept", PeerCommand.accept(instance: knock.addr),'
expect_red "every pairing verb takes the instance id" \
    "PeersPanelViewWiringTests/testTheTabsPairingRowRunsEveryVerbAgainstTheInstanceId"

echo "== 15. a scope change drops the lease's end"
mutate "Sources/TcrBar/PeersSettingsView.swift" \
    'PeerCommand.lend(peer: peer, scope: scope, terms: terms, end: end)' \
    'PeerCommand.lend(peer: peer, scope: scope, terms: terms)'
expect_red "changing a lease carries its end forward" \
    "PeersPanelViewWiringTests/testTheMacSheetsLeaseRowWritesEveryFlag"

echo "== 16. the URL handler logs the link itself"
mutate "Sources/TcrBar/TcrBarApp.swift" \
    'let shape = PeerJoinLink.redacted(url)' \
    'let shape = url.absoluteString'
expect_red "the handler logs the redacted shape, never the link" \
    "PeersPanelViewWiringTests/testTheUrlHandlerPassesTheWholeLinkAndLogsNeitherItNorAKey"

echo "== 17. the tcr:// scheme is not written into the bundle"
# shellcheck disable=SC2016  # the $names are Swift-side literals to MATCH in
# the script's own text, not expansions to perform here.
mutate "scripts/build-tcrbar.sh" \
    '<string>$peer_url_scheme</string>' \
    '<string>$url_scheme</string>'
expect_red "both schemes are registered" \
    "PeersPanelViewWiringTests/testTheBundleRegistersBothUrlSchemes"

echo "== 18. the account card indexes the lentTo map directly"
mutate "Sources/TcrBar/PanelV4/AccountsTabV4.swift" \
    'lentTo: PeerLease.leases(forAccountLabel: row.account.name, in: lentTo),' \
    'lentTo: lentTo[row.account.name] ?? [],'
expect_red "the card looks its leases up through the guarded helper" \
    "PeersPanelViewWiringTests/testTheAccountCardLooksUpItsLeasesThroughTheGuardedHelper"

echo "== 19. Advanced prints a cap literal beside the reported ones"
mutate "Sources/TcrBar/PeersSettingsView.swift" \
    '            peersRow(
                "The limits this Mac enforces on strangers' \
    '            LabeledContent("Macs shown at once") { Text("12") }
            peersRow(
                "The limits this Mac enforces on strangers'
expect_red "the Advanced pane holds no second copy of a cap" \
    "PeersPanelViewWiringTests/testTheAdvancedPaneReadsTheCapsObject"

echo "== 20. the Peers tab makes its own second peer reader"
mutate "Sources/TcrBar/FleetView.swift" \
    '                PeersView(
                    controller: peers, snapshotMode: snapshotMode,
                    onOpenSettings: onSettings)' \
    '                PeersView(snapshotMode: snapshotMode, onOpenSettings: onSettings)'
expect_red "one peer controller behind both surfaces" \
    "PeersPanelViewWiringTests/testTheAccountsTabAndThePeersTabShareOnePeerController"

echo "== 21. the lease ttl is read under one spelling only"
mutate "Sources/TcrBarCore/PeerLease.swift" \
    '            try c.decodeIfPresent(Int.self, forKey: .ttl)
            ?? c.decodeIfPresent(Int.self, forKey: .ttlS)
            ?? 300' \
    '            try c.decodeIfPresent(Int.self, forKey: .ttl) ?? 300'
expect_red "a lease ttl decodes under either spelling the producer uses" \
    "PeerListDocumentTests/testALeaseTtlDecodesUnderEitherSpellingTheProducerUses"

echo "== 22. an unknown allowance silently becomes the weekly one"
mutate "Sources/TcrBarCore/PeerLease.swift" \
    'self = PeerLeaseWindow(rawValue: raw) ?? .unknown' \
    'self = PeerLeaseWindow(rawValue: raw) ?? .week'
expect_red "an unknown window survives the decode and lends nothing" \
    "PeerListDocumentTests/testAnUnknownWindowSurvivesTheDecodeAndLendsNothing"

echo
if [[ $failures -eq 0 ]]; then
    echo "all 22 gates went red on their own mutation and green again after restore"
else
    echo "$failures gate(s) did not behave as a gate"
fi
exit "$failures"
