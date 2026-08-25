#!/usr/bin/env python3
"""Crop a --render-states panel PNG for the README, as 93f7056 did.

`ImageRenderer` cannot draw AppKit `.checkbox` toggles, so the three settings
rows at the bottom of the panel rasterise as a yellow prohibition placeholder.
The README shots are cut above them, at the blank gap below the action-button
row: find the first row carrying that placeholder's yellow, walk up to the last
run of background-only rows, and cut there.

Usage: crop-readme-panels.py <src.png> <dest.png>
Prints `<dest> <width>x<height> cut=<row>` so the crop point is reviewable.
"""
import sys
from PIL import Image


def is_placeholder_yellow(pixel) -> bool:
    red, green, blue = pixel[:3]
    return red > 200 and 140 < green < 230 and blue < 90


def main() -> int:
    src, dest = sys.argv[1], sys.argv[2]
    image = Image.open(src).convert("RGB")
    width, height = image.size
    background = image.getpixel((2, height // 2))

    # Scan the LEFT GUTTER only, where the placeholder glyph sits. A wider scan
    # matches an amber `near` quota bar, which starts around x=86 and would cut
    # the shot in the middle of the fleet — the check that catches that is
    # looking at the result, so the crop point is printed.
    first_yellow = None
    for y in range(height):
        run = sum(1 for x in range(20, 70) if is_placeholder_yellow(image.getpixel((x, y))))
        if run >= 10:
            first_yellow = y
            break
    if first_yellow is None:
        raise SystemExit(f"{src}: no settings placeholder found — nothing to crop above")

    def blank(y: int) -> bool:
        return all(
            max(abs(a - b) for a, b in zip(image.getpixel((x, y)), background)) <= 3
            for x in range(0, width, 2)
        )

    gap_top = first_yellow
    while gap_top > 0 and blank(gap_top - 1):
        gap_top -= 1
    # A few rows INTO the gap, not at its top: cutting flush against the last
    # drawn row shaves the bottom edge of the Quit button, which is what the
    # committed shots keep.
    cut = min(gap_top + 8, first_yellow)
    image.crop((0, 0, width, cut)).save(dest)
    print(f"{dest} {width}x{cut} cut={cut} gap={gap_top}..{first_yellow - 1}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
