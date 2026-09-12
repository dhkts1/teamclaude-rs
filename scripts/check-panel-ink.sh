#!/usr/bin/env bash
# check-panel-ink.sh — refuse SwiftUI's hierarchical foreground styles
# (`.secondary`, `.tertiary`, `.quaternary`) anywhere in the TcrBar views.
#
# The panel draws on its OWN surfaces: `Tok.panel` and `Tok.raised`, authored in
# OKLCH and measured for WCAG contrast by `scripts/tcrbar-palette.py`, which CI
# runs and which refuses a token below 4.5:1. A hierarchical style is not in
# that system. It resolves against the SYSTEM window background, so none of
# those measured ratios apply to it, and nothing was checking what it actually
# drew.
#
# What it actually drew, measured off `--render-states` bitmaps before this gate
# existed:
#
#   .tertiary    1.86:1 on the light panel, 2.26:1 on the dark
#   .secondary   3.86:1 on the light panel
#
# Both are below the 4.5:1 that every token in the same file clears, and
# `.tertiary` is close to invisible: it drew the 5-hour spend figure, the Fable
# weekly figure and the footer's server SHA. Thirteen runs were affected.
#
# The replacements are the panel's own ink scale, which says the same three
# levels out loud and is gated:
#
#   .secondary   -> Tok.inkDim     8.3:1 light, 9.7:1 dark
#   .tertiary    -> Tok.inkFaint   4.8:1 light, 5.8:1 dark
#
# Scope is `apps/macos/Sources/TcrBar`, the views that draw on those surfaces.
# `TcrBarCore` holds no views. Tests are exempt by name, for the same reason the
# --org gate exempts one file: a test that asserts the rule may have to write
# the thing down.
#
# Usage: scripts/check-panel-ink.sh [root]   (default: the repo root)

set -euo pipefail

root="${1:-$(git rev-parse --show-toplevel)}"
views="$root/apps/macos/Sources/TcrBar"

if [ ! -d "$views" ]; then
    echo "error: $views does not exist — has the package moved?" >&2
    exit 1
fi

# `foregroundStyle(.secondary)`, `foregroundColor(.tertiary)`, the ternary form
# `? Tok.spent : .secondary`, and the erased form `AnyShapeStyle(.tertiary)`.
#
# The trailing `\b` is what keeps `Tok.secondaryFont`, `Tok.secondaryDigitFont`
# and `Tok.secondaryLineSpacing` out: a word character follows `secondary` in
# each, so the boundary fails. An earlier draft also required whitespace before
# the dot, which silently missed the plainest form of all,
# `.foregroundStyle(.secondary)` — the fixture below is why that is not still
# true.
hits=$(
    grep -rnE \
        '(foregroundStyle|foregroundColor)\([^)]*\.(secondary|tertiary|quaternary)\b|AnyShapeStyle\(\.(secondary|tertiary|quaternary)\)' \
        "$views" 2>/dev/null || true
)

if [ -n "$hits" ]; then
    echo "error: a SwiftUI hierarchical style is used as ink on the panel." >&2
    echo "       It resolves against the system window background, not Tok.panel/Tok.raised," >&2
    echo "       so the measured contrast in Tokens.swift does not apply to it." >&2
    echo "       Use Tok.inkDim (secondary) or Tok.inkFaint (tertiary) instead." >&2
    echo "" >&2
    echo "$hits" >&2
    exit 1
fi

echo "check-panel-ink: clean (no hierarchical styles used as ink)"
