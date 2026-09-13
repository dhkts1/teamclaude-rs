#!/usr/bin/env bash
# Does the machine line's load clause actually TINT, or is the switch dead?
#
# The shipped fixture is `calm` (7.1 on 14 cores), so the render proves nothing
# about amber or red. This raises the fixture's load past 2x cores, renders,
# scans the machine-line band for `spent` (#ea5a56 dark), then restores the
# file byte-for-byte and re-renders.
set -uo pipefail

root="$(git -C "$(dirname "${BASH_SOURCE[0]}")" rev-parse --show-toplevel)"
states="$root/apps/macos/Sources/TcrBar/RenderStates.swift"
backup="/tmp/machine-tint-probe-$$"
out="/tmp/machine-tint-probe-out-$$"
mkdir -p "$out"
cp "$states" "$backup"
trap 'cp "$backup" "$states"' EXIT

sed -i 's/        loadAverage: 7.1,/        loadAverage: 31.0,/' "$states"
grep -n "loadAverage: 31.0," "$states" || { echo "PROBE BROKEN: no mutation applied"; exit 1; }

(cd "$root/apps/macos" && swift build >/dev/null 2>&1) || { echo "build failed"; exit 1; }
(cd "$root/apps/macos" && ./.build/debug/TcrBar --render-states "$out" >/dev/null 2>&1) ||
  { echo "render failed"; exit 1; }

scan() {
  python3 - "$1" <<'PY'
import sys
from PIL import Image
im = Image.open(sys.argv[1]).convert("RGB")
px = im.load()
w, h = im.size
# Rows 125 to 180 ONLY — the machine line's own two rendered lines.
#
# The first draft scanned the whole top tenth and reported "TINT FIRES" on a
# render whose machine line was grey: the summary line directly above ends in
# "31 hit the 600s timeout" in this exact red, 803 pixels of it at rows
# 102-117. A band that includes it is a gate that cannot fail. The control
# below is what caught that, and it stays in the script for the next person
# who widens the band.
target = (0xEA, 0x5A, 0x56)
hits = sum(
    1
    for y in range(125, min(180, h))
    for x in range(w)
    if all(abs(px[x, y][i] - target[i]) <= 18 for i in range(3))
)
print(f"spent-red pixels on the machine line: {hits}")
PY
}

echo "--- mutated fixture (load 31 of 14, past 2x cores): expect a few hundred"
scan "$out/17-tools-tab-auto-dark.png"
echo "--- CONTROL, shipped fixture (load 7.1/14, calm): expect 0"
scan "${TOOLS_TAB_RENDERS:-/tmp/tools-tab/after}/17-tools-tab-auto-dark.png"

cp "$backup" "$states"
trap - EXIT
cmp "$backup" "$states" && echo "restore: byte-identical"
rm -rf "$out" "$backup"
