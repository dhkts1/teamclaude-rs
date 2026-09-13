#!/usr/bin/env python3
"""Crop a --render-states panel PNG for the README, as 93f7056 did.

`ImageRenderer` cannot draw AppKit `.checkbox` toggles, so the keep-awake row at
the bottom of the panel rasterises as a yellow prohibition placeholder. The
README shots are cut above it, at the blank gap below the action-button row.

FINDING THE PLACEHOLDER. This used to scan the left gutter, `x` in [20, 70),
because a wider scan matches an amber `near` quota bar. That stopped working the
moment the keep-awake switch moved next to a label and the glyph slid to x≈170:
the script then raised "no settings placeholder found" on every scene, which
reads like "the panel has no settings rows" and is really "the probe looked in
the wrong place". It failed that way against `main`'s own render for anyone who
ran it.

So find it by what it IS rather than where it sat. The glyph is the only thing
on this panel drawn in saturated yellow WITH a red circle-slash through it: a
quota bar can be amber but never contains red, and a red bar is not yellow. Take
the first row carrying a long yellow run whose band also carries red, and the
answer holds wherever the row moves to next.

Usage: crop-readme-panels.py <src.png> <dest.png>
Prints `<dest> <width>x<height> cut=<row>` so the crop point is reviewable.
"""
import sys

from PIL import Image

# How far below a candidate row to look for the slash. The glyph is ~48 px tall
# at 2x and the slash crosses its middle, so half its height is enough.
SLASH_SEARCH_ROWS = 56
# A yellow run this long is a filled block, not an anti-aliased edge.
MIN_YELLOW_RUN = 40


def is_placeholder_yellow(pixel) -> bool:
    red, green, blue = pixel[:3]
    return red > 200 and 140 < green < 230 and blue < 90


def is_slash_red(pixel) -> bool:
    red, green, blue = pixel[:3]
    return red > 150 and green < 90 and blue < 90


def longest_yellow_run(image, y: int, width: int) -> int:
    run = best = 0
    for x in range(width):
        if is_placeholder_yellow(image.getpixel((x, y))):
            run += 1
            best = max(best, run)
        else:
            run = 0
    return best


def main() -> int:
    src, dest = sys.argv[1], sys.argv[2]
    image = Image.open(src).convert("RGB")
    width, height = image.size
    background = image.getpixel((2, height // 2))

    first_yellow = None
    for y in range(height):
        if longest_yellow_run(image, y, width) < MIN_YELLOW_RUN:
            continue
        band = range(y, min(y + SLASH_SEARCH_ROWS, height))
        if any(
            is_slash_red(image.getpixel((x, row)))
            for row in band
            for x in range(0, width, 2)
        ):
            first_yellow = y
            break
    if first_yellow is None:
        raise SystemExit(
            f"{src}: no settings placeholder found. That is a claim about this probe as "
            "much as about the image: check the panel still rasterises the keep-awake "
            "switch as a yellow no-entry glyph before believing there is nothing to cut."
        )

    # The panel draws its own 1 pt border, which at 2x is two solid columns down
    # BOTH edges of every row. Sampling from x=0 therefore made `blank` false for
    # every row in the image, in both appearances: the walk up into the gap never
    # moved and the cut landed flush on the keep-awake row. Measure the border
    # rather than guessing past it.
    inset = 0
    while inset < width // 4 and image.getpixel((inset, height // 2)) != background:
        inset += 1
    inset += 1  # and the antialiased column just inside it

    def blank(y: int) -> bool:
        return all(
            max(abs(a - b) for a, b in zip(image.getpixel((x, y)), background)) <= 3
            for x in range(inset, width - inset, 2)
        )

    # Walk up off the keep-awake ROW first, not just off the glyph. The switch
    # now sits beside a "Keep awake" label and across from Quit, so the rows
    # immediately above the glyph are still that row's own ink and there is no
    # blank gap there to find. Clearing the band puts us in the gap the cut
    # belongs in.
    band_top = first_yellow
    while band_top > 0 and not blank(band_top - 1):
        band_top -= 1

    gap_top = band_top
    while gap_top > 0 and blank(gap_top - 1):
        gap_top -= 1
    # A few rows INTO the gap, not at its top: cutting flush against the last
    # drawn row shaves the bottom edge of the Quit button, which is what the
    # committed shots keep.
    cut = min(gap_top + 8, band_top)
    image.crop((0, 0, width, cut)).save(dest)
    print(f"{dest} {width}x{cut} cut={cut} gap={gap_top}..{first_yellow - 1}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
