#!/usr/bin/env python3
"""Search for a light-mode status ramp that satisfies every constraint at once.

Hand-tuning was going in circles: darkening amber to reach 3:1 pushed it into
green, and deepening red to separate it from green pushed red out of the sRGB
gamut. Those constraints are coupled, so solve them together instead of one at a
time.

Constraints, all simultaneous:
  - every hue >= 3.0:1 against BOTH the panel and a raised row (WCAG 1.4.11)
  - no sRGB gamut clipping
  - >= 1.25:1 luminance separation between ready/near, ready/spent, near/spent
    (so the red/green pair survives colour-vision deficiency and greyscale)
  - chroma as high as the other constraints allow, so the palette is not muddy
"""
import sys

sys.path.insert(0, __import__("os").path.dirname(__file__))
from importlib import import_module

pal = import_module("tcrbar-palette".replace("-", "_")) if False else None

# Re-import the conversion helpers without executing main().
import importlib.util
import os

spec = importlib.util.spec_from_file_location(
    "pal", os.path.join(os.path.dirname(__file__), "tcrbar-palette.py")
)
pal = importlib.util.module_from_spec(spec)
spec.loader.exec_module(pal)

PANEL = pal.LIGHT_SURFACES["panel"]
RAISED = pal.LIGHT_SURFACES["raised"]


def ok_token(L, C, H, floor=3.0):
    _, clipped = pal.oklch_to_hex(L, C, H)
    if clipped:
        return False
    return min(pal.contrast((L, C, H), PANEL), pal.contrast((L, C, H), RAISED)) >= floor


def best_chroma(L, H, floor=3.0):
    """Highest chroma that stays in gamut and clears the contrast floor."""
    best = None
    c = 0.02
    while c <= 0.30:
        if ok_token(L, c, H, floor):
            best = c
        c += 0.005
    return best


def main():
    hues = {"ready": 150, "near": 70, "spent": 25}
    # Candidate lightnesses per role, ordered so severity reads as weight.
    grid = {r: [round(0.40 + i * 0.005, 3) for i in range(70)] for r in hues}

    solutions = []
    for Ls in grid["spent"]:
        for Lr in grid["ready"]:
            for Ln in grid["near"]:
                if not (Ls < Lr < Ln):
                    continue
                sep = min(
                    pal.contrast((Lr, 0.1, 150), (Ls, 0.1, 25)),
                    pal.contrast((Ln, 0.1, 70), (Lr, 0.1, 150)),
                    pal.contrast((Ln, 0.1, 70), (Ls, 0.1, 25)),
                )
                if sep < 1.25:
                    continue
                cs = {r: best_chroma(L, hues[r])
                      for r, L in (("spent", Ls), ("ready", Lr), ("near", Ln))}
                if any(c is None for c in cs.values()):
                    continue
                solutions.append((sum(cs.values()), sep, Ls, Lr, Ln, cs))

    if not solutions:
        print("NO SOLUTION under these constraints")
        return 1

    # Prefer the most chromatic palette, then the widest separation.
    solutions.sort(key=lambda s: (-s[0], -s[1]))
    total, sep, Ls, Lr, Ln, cs = solutions[0]
    print(f"best: total chroma {total:.3f}, min separation {sep:.2f}:1\n")
    for role, L in (("ready", Lr), ("near", Ln), ("spent", Ls)):
        C = cs[role]
        hexv, _ = pal.oklch_to_hex(L, C, hues[role])
        cp = pal.contrast((L, C, hues[role]), PANEL)
        cr = pal.contrast((L, C, hues[role]), RAISED)
        print(f'    "{role}":{" " * (12 - len(role))}({L:.3f}, {C:.3f}, {hues[role]:3d}),'
              f"   {hexv}  panel {cp:.2f}:1  raised {cr:.2f}:1")
    return 0


if __name__ == "__main__":
    sys.exit(main())
