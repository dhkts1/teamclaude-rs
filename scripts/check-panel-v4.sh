#!/usr/bin/env bash
#
# The v4 panel's views may not write a number, and may not reach for a pre-v4
# geometry token.
#
# `apps/macos/Sources/TcrBar/PanelV4/` is a transcription of
# `docs/design/panel-tabs-mockup.html`'s `<style>` block: one CSS declaration,
# one `V4` constant, `data/plans/v4-spec.md` as the sheet in between. Eleven
# adaptation rounds before it drifted by exactly one mechanism — a 16 written
# here because it looked right, an 8 there because the thing above it had one —
# each locally defensible and collectively a different design from the one that
# was approved. A literal in a view is that drift's only entry point, so it is
# refused at the door; `V4.swift` is the one file allowed to hold numbers,
# because holding them is what it is for.
#
# `Tok.gutter`, `Tok.rowSpacing` and the rest are the PRE-v4 sheet. They are
# still correct for the views that have not migrated (Sessions, Tools, the
# legacy panel behind `TCRBAR_LEGACY_PANEL=1`), which is why this gate is
# scoped to the `PanelV4/` directory rather than to the whole target. Colour
# tokens (`Tok.ok`, `Tok.cardLine`, `Tok.panel`, …) are NOT geometry and are
# exactly what `PanelV4/` is supposed to use: they are bound to the gated
# palette that `scripts/tcrbar-palette.py` emits.
#
# Usage: scripts/check-panel-v4.sh [root]   (default: the repo root)
#
# Exit 0 clean, 1 on any finding. Output is `file:line: severity: summary`.

set -euo pipefail

root="${1:-$(git rev-parse --show-toplevel)}"
views="$root/apps/macos/Sources/TcrBar/PanelV4"
sheet="$views/V4.swift"

if [ ! -d "$views" ]; then
    echo "error: $views does not exist — has PanelV4 moved?" >&2
    exit 1
fi

# A positive control. An empty result from a search is a claim about the search,
# not about the tree: if the glob stops matching (a rename, a move), every check
# below reports clean for the wrong reason.
count=$(find "$views" -name '*.swift' -type f | wc -l | tr -d ' ')
if [ "$count" -lt 2 ]; then
    echo "error: found $count Swift files under $views — the gate is not looking at the panel." >&2
    exit 1
fi
if [ ! -f "$sheet" ]; then
    echo "error: $sheet is missing — there is no sheet to check the views against." >&2
    exit 1
fi

findings=""

# A numeric literal in a call that positions, sizes or spaces something.
#
# Only these modifiers, not "any digit": a view legitimately writes `0` for a
# stack spacing it is overriding, an index, a `prefix(3)`, an opacity it takes
# from the sheet. What may never be written by hand is a POINT value, and every
# one of them enters through one of these.
geometry='(padding|frame|offset|spacing|lineWidth|cornerRadius|lineSpacing|inset|tracking|kerning)'
literals=$(
    grep -nE "\.?${geometry}\(([^)]*[^A-Za-z0-9_.])?-?[0-9]+(\.[0-9]+)?([,)]| )" \
        "$views"/*.swift 2>/dev/null |
        grep -v '^[^:]*V4\.swift:' |
        grep -vE ':\s*(///|//)' || true
)
if [ -n "$literals" ]; then
    while IFS= read -r hit; do
        findings="${findings}${hit%%:*}:$(echo "$hit" | cut -d: -f2): error: v4: a size written by hand — put it in V4.swift with its CSS declaration
"
    done <<<"$literals"
fi

# A pre-v4 GEOMETRY token. The colour tokens beside them are wanted.
stale=$(
    grep -nE 'Tok\.(gutter|rowSpacing|tightSpacing|panelWidth|cardPadding|hairline|cornerRadius|barHeight|pillPadding|iconSize)\b' \
        "$views"/*.swift 2>/dev/null |
        grep -vE ':\s*(///|//)' || true
)
if [ -n "$stale" ]; then
    while IFS= read -r hit; do
        findings="${findings}${hit%%:*}:$(echo "$hit" | cut -d: -f2): error: v4: a pre-v4 geometry token — the v4 sheet has its own value for this
"
    done <<<"$stale"
fi

if [ -n "$findings" ]; then
    printf '%s' "$findings" >&2
    echo "" >&2
    echo "check-panel-v4: $(printf '%s' "$findings" | grep -c . ) finding(s) in $views" >&2
    exit 1
fi

echo "check-panel-v4: clean ($count files, no hand-written sizes, no pre-v4 geometry tokens)"
