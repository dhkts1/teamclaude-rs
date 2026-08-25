#!/usr/bin/env python3
"""Watch each of round two's new tests fail against the behaviour it guards.

A test that has only ever passed proves nothing about what it guards (CLAUDE.md,
"Verifying a change"). This applies one mutation at a time — reverting the fixed
line to what it did before — runs ONLY the test that should catch it, records the
failure line, and puts the file back.

Reverts by re-writing the exact bytes it replaced, never by `git checkout`: a
checkout of a path would also discard any sibling's uncommitted work in that file.
The fix is committed first, so an interrupted run loses nothing either way.

Usage: mutate-panel-round2.py [item ...]     (default: every item)
Prints one `item: EXPECTED-FAIL|UNEXPECTED-PASS ...` line per mutation.
"""
import pathlib
import re
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
MACOS = ROOT / "apps/macos"
CORE = MACOS / "Sources/TcrBarCore"
FLEET = CORE / "FleetStatus.swift"
PANEL = CORE / "PanelHeight.swift"

# item -> (file, old bytes to find, mutated bytes, test filter)
MUTATIONS = {
    # Finding 2: drop the render condition, so a stale measurement keeps being
    # charged to the list after the spend line stops rendering.
    "2": (
        PANEL,
        "        guard lineIsDrawn else { return 0 }\n        return lineHeight - oneLineHeight",
        "        return lineHeight - oneLineHeight",
        "testAHeaderThatStopsDrawingItsSpendLineGivesTheBudgetBack",
    ),
    # Finding 3: drop the control-row separator from the viewport sum.
    "3": (
        PANEL,
        "        let separator = controlHairline.map { spacing + $0 } ?? 0",
        "        let separator: CGFloat = 0",
        "testAPinnedControlRowsSeparatorIsInTheSum",
    ),
    # Finding 4: go back to dropping the rate with nothing said about the hour.
    "4": (
        FLEET,
        '        guard let unpriced = lastHourUnpricedRequests, unpriced > 0 else { return nil }\n        return "\\(unpriced) unpriced this hour"',
        "        return nil",
        "testAnUnpriceableHourSaysSoEvenWhenTheDayIsEmpty",
    ),
    # Finding 5: stop carrying the merged label's unpriced requests out.
    "5": (
        FLEET,
        "                        partial: $0.value.unpricedRequests > 0)",
        "                        partial: false)",
        "testAMergedLabelSaysItsPercentIsAFloor",
    ),
    # Finding 6: take the decimals from the RAW unit again, after the band was
    # already promoted from the rounded one.
    "6": (
        FLEET,
        '        return String(format: "%.\\(band.decimals)f", unit) + tokenBands[band.unit].suffix',
        '        return String(format: "%.\\(abs(unit) < 10 ? 1 : 0)f", unit) + tokenBands[band.unit].suffix',
        "testTokensPickTheirDecimalsAfterRoundingToo",
    ),
    # Finding 7: end the ladder at G again.
    "7": (
        FLEET,
        '        (1, ""), (1_000, "k"), (1_000_000, "M"), (1_000_000_000, "G"),\n        (1_000_000_000_000, "T"),',
        '        (1, ""), (1_000, "k"), (1_000_000, "M"), (1_000_000_000, "G"),',
        "testTheTopBandRoundsLikeEveryOtherBand",
    ),
    # Finding 8: break the FLOOR the panel runs on, so a long enough header eats
    # the whole list.
    #
    # Deliberately the arithmetic and not the constant. Mutating
    # `panelMinListHeight` itself cannot fail and is not a control: the number
    # has exactly one home now, and both the panel and the assertion read that
    # one — they move together, which is the entire point of the move. What the
    # finding asked for was a gate that cannot pass against a stale copy of the
    # value, and the way to get that is to leave no copy, not to pin `120` in a
    # test file and re-create the duplicate one directory over.
    "8": (
        PANEL,
        "        max(minimum, cap - max(0, headerOverflow))",
        "        cap - max(0, headerOverflow)",
        "testAnAbsurdHeaderStillLeavesAScrollableList",
    ),
    # Finding 9: take the re-pass out of the one helper both formatters share,
    # which must red BOTH of their gates at once.
    "9": (
        FLEET,
        "        band(printedMagnitude(band(magnitude), magnitude))",
        "        band(magnitude)",
        "testBothFormattersShareTheSameRoundThenBandRule",
    ),
    # The Fable slot: render `n/a` for a window nobody measured, which is the
    # one thing the whole slot must not do.
    "fable": (
        FLEET,
        "        guard let sevenDayOi else { return nil }\n        let figure = \"fable \\(QuotaFormat.percent(sevenDayOi))\"",
        '        let figure = "fable \\(QuotaFormat.percent(sevenDayOi))"',
        "testAnUnmeasuredWindowDrawsNothingAtAll",
    ),
}


def run(test_filter):
    """Run one test and return (passed, first failure line)."""
    result = subprocess.run(
        ["swift", "test", "--filter", test_filter],
        cwd=MACOS, capture_output=True, text=True)
    failures = [
        line.strip() for line in result.stdout.splitlines() + result.stderr.splitlines()
        if re.search(r"\.swift:\d+: error:", line)
    ]
    # A filter that matches nothing still exits 0 with "Executed 0 tests", which
    # would read as a pass — so the count has to be non-zero, not merely present.
    executed = any(
        re.search(r"Executed [1-9]\d* test", line)
        for line in result.stdout.splitlines() + result.stderr.splitlines())
    return result.returncode == 0, failures, executed


def main():
    items = sys.argv[1:] or list(MUTATIONS)
    worst = 0
    for item in items:
        path, old, new, test_filter = MUTATIONS[item]
        text = path.read_text(encoding="utf-8")
        if text.count(old) != 1:
            print(f"{item}: ANCHOR-MISS {path.name} matched {text.count(old)} times "
                  "— the code moved and this mutation did not")
            worst = 2
            continue

        # Control first: the test must PASS on the committed fix, or a failure
        # under mutation proves nothing about the mutation.
        passed, _, executed = run(test_filter)
        if not (passed and executed):
            print(f"{item}: CONTROL-FAILED {test_filter} does not pass unmutated")
            worst = 2
            continue

        path.write_text(text.replace(old, new), encoding="utf-8")
        try:
            passed, failures, executed = run(test_filter)
        finally:
            path.write_text(text, encoding="utf-8")

        if not executed:
            print(f"{item}: NO-SUCH-TEST {test_filter} matched nothing")
            worst = 2
        elif passed:
            print(f"{item}: UNEXPECTED-PASS {test_filter} survived the mutation "
                  "— it does not guard what it claims")
            worst = 2
        else:
            print(f"{item}: EXPECTED-FAIL {test_filter}")
            for line in failures[:2]:
                print(f"    {line}")

    # And the tree is back the way it was.
    dirty = subprocess.run(
        ["git", "-C", str(ROOT), "status", "--porcelain", "--",
         str(FLEET.relative_to(ROOT)), str(PANEL.relative_to(ROOT))],
        capture_output=True, text=True).stdout.strip()
    if dirty:
        print(f"RESTORE-FAILED: {dirty}")
        worst = 2
    else:
        print("restored: both sources match HEAD")
    return worst


if __name__ == "__main__":
    raise SystemExit(main())
