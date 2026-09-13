#!/usr/bin/env python3
"""Search for a light-mode status ramp that satisfies every constraint at once.

Hand-tuning was going in circles: darkening amber to reach the floor pushed it
into green, and deepening red to separate it from green pushed red out of the
sRGB gamut. Those constraints are coupled, so solve them together instead of
one at a time.

The floor is 4.5:1, not 3.0:1 (review #6,
`data/plans/interface-review-2026-09-13.md`): a status hue here is a pill
label, a summary clause and a reset caption -- body text wearing a hue, not a
bar or a dot beside it -- so it carries the same obligation the ink scale
does, in both appearances.

Three groups, each with its own coupling:

  - `ready` / `near` / `spent`, the traffic-light triple: >= 4.5:1 on both
    surfaces, no gamut clipping, and >= 1.25:1 pairwise luminance separation
    using each role's ACTUAL solved chroma (a fixed proxy chroma understates
    how close two hues sit once each is pushed to its own best chroma --
    which is exactly how a first pass at this re-solve put ready 1.24:1 from
    near, under the floor, while its own proxy metric read 1.26:1).
  - `unmeasured` / `awake`, the two cool off-scale tokens: same floor and
    gamut rule, plus >= 1.25:1 from each other -- the one pair review #6's
    own gate still checks for this appearance.
  - `disabled`, alone: near-neutral by design (chroma 0.004, same as the dark
    token), so it is darkened only as far as the floor needs, at its
    original chroma, rather than maximised toward a hue it was never meant
    to carry.
"""
import sys

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

FLOOR = 4.5


def ok_token(L, C, H, floor=FLOOR):
    _, clipped = pal.oklch_to_hex(L, C, H)
    if clipped:
        return False
    return min(pal.contrast((L, C, H), PANEL), pal.contrast((L, C, H), RAISED)) >= floor


def best_chroma(L, H, floor=FLOOR):
    """Highest chroma that stays in gamut and clears the contrast floor."""
    best = None
    c = 0.02
    while c <= 0.30:
        if ok_token(L, c, H, floor):
            best = c
        c += 0.005
    return best


def solve_triple():
    hues = {"ready": 150, "near": 70, "spent": 25}
    grid = {r: [round(0.20 + i * 0.005, 3) for i in range(120)] for r in hues}

    solutions = []
    for Ls in grid["spent"]:
        for Lr in grid["ready"]:
            for Ln in grid["near"]:
                if not (Ls < Lr < Ln):
                    continue
                cs = {}
                for r, L in (("spent", Ls), ("ready", Lr), ("near", Ln)):
                    c = best_chroma(L, hues[r])
                    if c is None:
                        cs = None
                        break
                    cs[r] = c
                if cs is None:
                    continue
                sep = min(
                    pal.contrast((Lr, cs["ready"], 150), (Ls, cs["spent"], 25)),
                    pal.contrast((Ln, cs["near"], 70), (Lr, cs["ready"], 150)),
                    pal.contrast((Ln, cs["near"], 70), (Ls, cs["spent"], 25)),
                )
                if sep < 1.25:
                    continue
                solutions.append((sum(cs.values()), sep, Ls, Lr, Ln, cs))

    if not solutions:
        return None
    solutions.sort(key=lambda s: (-s[0], -s[1]))
    return solutions[0]


def solve_unmeasured_and_awake():
    """Jointly, both maximising chroma per candidate lightness -- unlike
    `disabled` below, neither of these is meant to read as neutral, so
    darkening at a FIXED original chroma would leave headroom on the table."""
    HUE_UNMEASURED, HUE_AWAKE = 230, 195
    grid = [round(0.20 + i * 0.005, 3) for i in range(120)]

    per_l_um = {L: best_chroma(L, HUE_UNMEASURED) for L in grid}
    per_l_aw = {L: best_chroma(L, HUE_AWAKE) for L in grid}

    solutions = []
    for Lu, Cu in per_l_um.items():
        if Cu is None:
            continue
        for La, Ca in per_l_aw.items():
            if Ca is None:
                continue
            sep = pal.contrast((La, Ca, HUE_AWAKE), (Lu, Cu, HUE_UNMEASURED))
            if sep < 1.25:
                continue
            solutions.append((Cu + Ca, sep, Lu, Cu, La, Ca))

    if not solutions:
        return None
    solutions.sort(key=lambda s: (-s[0], -s[1]))
    return solutions[0]


def solve_disabled(orig_chroma=0.004, hue=255):
    """Largest (least-darkened) L at the token's own chroma that still
    clears the floor."""
    L = 0.70
    while L >= 0.20:
        if ok_token(L, orig_chroma, hue):
            return L
        L -= 0.005
    return None


def main():
    triple = solve_triple()
    if triple is None:
        print("NO SOLUTION under these constraints (triple)")
        return 1
    total, sep, Ls, Lr, Ln, cs = triple
    hues = {"ready": 150, "near": 70, "spent": 25}
    print(f"floor {FLOOR}:1\n")
    print(f"triple: total chroma {total:.3f}, min separation {sep:.3f}:1\n")
    for role, L in (("ready", Lr), ("near", Ln), ("spent", Ls)):
        C = cs[role]
        hexv, _ = pal.oklch_to_hex(L, C, hues[role])
        cp = pal.contrast((L, C, hues[role]), PANEL)
        cr = pal.contrast((L, C, hues[role]), RAISED)
        print(f'    "{role}":{" " * (12 - len(role))}({L:.3f}, {C:.3f}, {hues[role]:3d}),'
              f"   {hexv}  panel {cp:.2f}:1  raised {cr:.2f}:1")

    pair = solve_unmeasured_and_awake()
    if pair is None:
        print("\nNO SOLUTION under these constraints (unmeasured/awake pair)")
        return 1
    total_pair, sep_pair, Lu, Cu, La, Ca = pair
    print(f"\nunmeasured/awake pair: total chroma {total_pair:.3f}, "
          f"separation {sep_pair:.3f}:1\n")
    for role, (L, C, H) in (("unmeasured", (Lu, Cu, 230)), ("awake", (La, Ca, 195))):
        hexv, _ = pal.oklch_to_hex(L, C, H)
        cp = pal.contrast((L, C, H), PANEL)
        cr = pal.contrast((L, C, H), RAISED)
        print(f'    "{role}":{" " * (12 - len(role))}({L:.3f}, {C:.3f}, {H:3d}),'
              f"   {hexv}  panel {cp:.2f}:1  raised {cr:.2f}:1")

    Ld = solve_disabled()
    if Ld is None:
        print("\nNO SOLUTION under these constraints (disabled)")
        return 1
    hexv, _ = pal.oklch_to_hex(Ld, 0.004, 255)
    cp = pal.contrast((Ld, 0.004, 255), PANEL)
    cr = pal.contrast((Ld, 0.004, 255), RAISED)
    print(f"\ndisabled (own chroma 0.004, darkened only as far as the floor needs):")
    print(f'    "disabled":   ({Ld:.3f}, 0.004, 255),   {hexv}  '
          f"panel {cp:.2f}:1  raised {cr:.2f}:1")
    return 0


if __name__ == "__main__":
    sys.exit(main())
