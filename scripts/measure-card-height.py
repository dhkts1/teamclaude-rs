#!/usr/bin/env python3
"""Measure account-card heights in a --render-states PNG.

A card is drawn as `Tok.raised` fill inside a hairline border on the panel
background, so a vertical scan down one column inside the cards finds each
card as a contiguous run of rows whose colour differs from the panel's.

Usage: measure-card-height.py <png> [column]
Prints one line per band: `band <top>..<bottom> height=<px> (<pt>pt at 2x)`.
"""
import sys
from PIL import Image


def main() -> int:
    path = sys.argv[1]
    column = int(sys.argv[2]) if len(sys.argv) > 2 else 40
    image = Image.open(path).convert("RGB")
    width, height = image.size
    background = image.getpixel((2, height // 2))
    runs = []
    start = None
    for y in range(height):
        pixel = image.getpixel((column, y))
        differs = max(abs(a - b) for a, b in zip(pixel, background)) > 3
        if differs and start is None:
            start = y
        elif not differs and start is not None:
            runs.append((start, y - 1))
            start = None
    if start is not None:
        runs.append((start, height - 1))
    print(f"{path} {width}x{height} background={background} column={column}")
    for top, bottom in runs:
        px = bottom - top + 1
        print(f"band {top}..{bottom} height={px} ({px / 2:g}pt at 2x)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
