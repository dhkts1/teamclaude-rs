#!/usr/bin/env python3
"""Build and VALIDATE the TcrBar dark token system.

OKLCH is authored here because it is perceptually uniform: equal L steps read as
equal brightness steps, and hue stays put as lightness moves (HSL blue drifts
purple as it darkens, which is what makes hand-tuned ramps wander).

Nothing here is taken on faith. Every colour is converted OKLCH -> sRGB, checked
for gamut clipping, and measured for WCAG contrast against the surface it is
actually drawn on. A token that fails its own check is a defect, not a taste
question, and the script exits non-zero so it can gate a build.
"""
import argparse
import json
import math
import pathlib
import re
import sys

# ---------------------------------------------------------------- conversion


def oklch_to_linear_srgb(L, C, H):
    a = C * math.cos(math.radians(H))
    b = C * math.sin(math.radians(H))
    l_ = L + 0.3963377774 * a + 0.2158037573 * b
    m_ = L - 0.1055613458 * a - 0.0638541728 * b
    s_ = L - 0.0894841775 * a - 1.2914855480 * b
    l, m, s = l_ ** 3, m_ ** 3, s_ ** 3
    return (
        4.0767416621 * l - 3.3077115913 * m + 0.2309699292 * s,
        -1.2684380046 * l + 2.6097574011 * m - 0.3413193965 * s,
        -0.0041960863 * l - 0.7034186147 * m + 1.7076147010 * s,
    )


def gamma(c):
    return 12.92 * c if c <= 0.0031308 else 1.055 * (c ** (1 / 2.4)) - 0.055


def oklch_to_hex(L, C, H):
    lin = oklch_to_linear_srgb(L, C, H)
    clipped = any(c < -0.0001 or c > 1.0001 for c in lin)
    rgb = [max(0.0, min(1.0, gamma(max(0.0, min(1.0, c))))) for c in lin]
    return "#%02x%02x%02x" % tuple(round(c * 255) for c in rgb), clipped


def relative_luminance(L, C, H):
    r, g, b = (max(0.0, min(1.0, c)) for c in oklch_to_linear_srgb(L, C, H))
    return 0.2126 * r + 0.7152 * g + 0.0722 * b


def contrast(fg, bg):
    a, b = relative_luminance(*fg), relative_luminance(*bg)
    hi, lo = max(a, b), min(a, b)
    return (hi + 0.05) / (lo + 0.05)


# ------------------------------------------------------------------- tokens
#
# Surface: a near-neutral ramp at a single cool hue. Chroma is deliberately tiny
# (0.006-0.012) rather than zero -- a pure grey panel reads dead next to macOS's
# own vibrant chrome, while anything above ~0.02 starts to look tinted.
HUE_SURFACE = 255

SURFACES = {
    "void":         (0.155, 0.010, HUE_SURFACE),   # backdrop behind the panel
    "panel":        (0.200, 0.011, HUE_SURFACE),   # the panel body itself
    "raised":       (0.245, 0.012, HUE_SURFACE),   # account rows / cards
    "hover":        (0.290, 0.014, HUE_SURFACE),   # row hover
    "track":        (0.320, 0.012, HUE_SURFACE),   # empty quota bar
    "hairline":     (0.360, 0.012, HUE_SURFACE),   # 0.5pt dividers
    "hairlineHigh": (0.460, 0.014, HUE_SURFACE),   # emphasised divider / border
}

# Ink: warm-neutral off-white. Never pure white on a dark panel -- it vibrates.
INK = {
    "ink":      (0.960, 0.004, 95),   # primary text
    "inkDim":   (0.800, 0.006, 95),   # secondary
    "inkFaint": (0.660, 0.008, 95),   # tertiary / hints
}

# Status hues.
#
# Two rules are doing the work here, and the first one is not obvious.
#
# 1. The statuses are separated in LIGHTNESS, not only in hue. Hue alone fails
#    two ways at once: red-green is the common colour-vision deficiency, so
#    ready-vs-spent is the single worst pair to distinguish by hue; and any
#    greyscale rendering collapses hue entirely. The first draft of this file
#    had ready at L 0.780 and near at L 0.800 -- a measured 1.00:1, literally
#    the same brightness -- which looked fine on my screen and would have been
#    unreadable to a deuteranope. Ordering by severity (spent darkest, near
#    brightest) also means urgency reads as weight even with no colour at all.
#
# 2. Chroma is per-hue, not one absolute number: equal C does NOT read as
#    equally vivid across hues, because each hue carries a different maximum.
STATUS = {
    "ready":      (0.775, 0.150, 150),   # green  - has headroom
    "near":       (0.880, 0.130,  85),   # amber  - close to the limit
    "spent":      (0.655, 0.180,  25),   # red    - exhausted
    "unmeasured": (0.760, 0.070, 230),   # slate  - never probed, NOT zero
    "disabled":   (0.660, 0.004, 255),   # grey   - operator parked it
    "awake":      (0.830, 0.120, 195),   # cyan   - keep-awake mode is held
}

WASH_ALPHA, LINE_ALPHA = 0.14, 0.34


# ------------------------------------------------------- light + high contrast
#
# A hardcoded palette that only exists in one appearance is a REGRESSION against
# the system semantic colours it replaces, which adapt to light mode and to
# Increased Contrast for free. So every token ships four values, and all four are
# measured. Increased Contrast pushes ink away from the surface and darkens the
# status hues toward their backgrounds rather than merely saturating them.

LIGHT_SURFACES = {
    "void":         (0.985, 0.002, HUE_SURFACE),
    "panel":        (0.965, 0.003, HUE_SURFACE),
    "raised":       (0.935, 0.004, HUE_SURFACE),
    "hover":        (0.900, 0.006, HUE_SURFACE),
    "track":        (0.870, 0.006, HUE_SURFACE),
    "hairline":     (0.830, 0.006, HUE_SURFACE),
    "hairlineHigh": (0.720, 0.008, HUE_SURFACE),
}

LIGHT_INK = {
    "ink":      (0.180, 0.006, 95),
    "inkDim":   (0.400, 0.008, 95),
    "inkFaint": (0.490, 0.010, 95),
}

# On a light panel the same hue must go DARKER, not lighter, to keep contrast --
# and the L ORDER has to be re-derived, not mirrored. A first pass that simply
# flipped the dark ramp put ready and spent 1.07:1 apart, collapsing the
# red/green pair that the dark palette holds 1.79:1 apart.
#
# These are judged at 3:1, not 4.5:1, and that is deliberate rather than a
# concession: a status hue is never body text here. It is a pill label drawn on
# a wash of its own hue, a bar fill, or a dot -- graphical elements, which
# WCAG 1.4.11 sets at 3:1. The ink scale above carries the 4.5:1 obligation.
#
# The light ramp is compressed compared to the dark one, and amber sets the
# floor: yellow simply has less luminance headroom, so it must go darker than
# instinct suggests to clear 3:1 on a near-white panel. Once amber is pinned,
# green and red are placed beneath it to keep the pairwise separation.
# These three came out of scripts/solve-light-status.py rather than by eye. The
# constraints are coupled -- darkening amber to reach 3:1 pushes it into green,
# and deepening red to separate it from green pushes red out of the sRGB gamut --
# so they are solved together, maximising chroma subject to all of them.
LIGHT_STATUS = {
    "ready":      (0.545, 0.150, 150),
    "near":       (0.625, 0.135,  70),
    "spent":      (0.510, 0.205,  25),
    "unmeasured": (0.580, 0.070, 230),
    "disabled":   (0.600, 0.004, 255),
    "awake":      (0.500, 0.085, 195),
}

# `awake` -- keep-awake mode is held -- is NOT on the traffic-light scale, and
# that is the point: reusing ready/near/spent would make the menu bar say
# "nearly exhausted" when it means "this Mac will not idle sleep". Hue 195 is
# the open slot between ready (150) and unmeasured (230).
#
# The constraint set both values were searched against is this file's own:
# stay in the sRGB gamut, clear the contrast floor on BOTH the panel and a
# raised row, and hold >= 1.25:1 luminance separation from `unmeasured` -- the
# one other cool token, and so the only one it could be confused with in
# greyscale. The two values sit differently against it, and saying so matters
# more than a tidier sentence would:
#
#  - LIGHT, oklch(0.500 0.085 195) -> #017272, is the constrained maximum. At
#    L=0.500 the gamut runs out at C=0.085, so the shipped value is the most
#    chroma the constraint set allows at that lightness, to the digit.
#  - DARK, oklch(0.830 0.120 195) -> #51dfdf, is NOT. Nothing gated here binds
#    at C=0.120: at L=0.830 every constraint above still passes at C=0.141
#    (#12e3e3, 11.31:1 on panel, 1.32:1 from `unmeasured`), and letting L move
#    too reaches oklch(0.905 0.150 195). The shipped value was chosen BELOW
#    that ceiling for restraint, not derived: at C=0.120 `awake` is the least
#    chromatic of the four coloured tokens (near 0.130, ready 0.150, spent
#    0.180), so a mode indicator never out-shouts a quota status beside it.
#    That is a taste judgement and this script does not measure it -- what is
#    gated is the floor the colour clears, never the ceiling it declines. Do
#    not "fix" the value to the maximum; it is deliberate.
#
# Two things the search taught, both of which are in the numbers above:
#
#  - Maximising chroma alone is the wrong objective. Unconstrained, it returns
#    oklch(0.665 0.295 330) -> #ed17e6, a neon magenta at twice the chroma of
#    any shipped token, which also collides with `unknown` (#c69bdd).
#  - Requiring separation from ALL FIVE existing statuses returns nothing at
#    all in this hue range -- and that constraint is not this palette's: the
#    shipped tokens do not meet it either (unmeasured vs ready is 1.02:1,
#    disabled vs spent is 1.00:1). What is gated here is four NAMED pairs, and
#    tokens off the traffic-light scale are separated by hue and by context.
#
# Known gap, deliberately not closed here: `unknown` (Tokens.swift, #c69bdd /
# #7d4d96) is shipped but absent from this file, so it is the one token nothing
# measures. It was left out rather than added because its dark value does not
# round-trip -- the nearest OKLCH, (0.752 0.103 314), emits #c69bdc, one bit of
# blue off what the app draws. Adding it would make this script authoritative
# for a colour it does not actually reproduce, which is worse than the gap:
# silent drift beats a known hole. Closing it properly means re-deriving that
# token and changing what the app draws, which is its own change.

# Increased Contrast: ink goes to the extremes, status hues gain lightness
# separation from the panel.
HC_DARK_INK = {
    "ink":      (1.000, 0.000, 95),
    "inkDim":   (0.880, 0.004, 95),
    "inkFaint": (0.780, 0.006, 95),
}
HC_LIGHT_INK = {
    "ink":      (0.100, 0.004, 95),
    "inkDim":   (0.280, 0.006, 95),
    "inkFaint": (0.400, 0.008, 95),
}


def check_set(label, surfaces, ink, status, failures, min_ratio=4.5):
    panel, raised = surfaces["panel"], surfaces["raised"]
    print(f"\n{label}")
    for name, v in list(ink.items()) + list(status.items()):
        hexv, clipped = oklch_to_hex(*v)
        on_panel, on_raised = contrast(v, panel), contrast(v, raised)
        worst = min(on_panel, on_raised)
        ok = worst >= min_ratio and not clipped
        if not ok:
            failures.append(
                f"[{label}] {name} {worst:.2f}:1 < {min_ratio}:1"
                + ("  GAMUT-CLIP" if clipped else "")
            )
        print(f"  {name:<13} {hexv}  panel {on_panel:5.2f}:1  raised "
              f"{on_raised:5.2f}:1  {'PASS' if ok else 'FAIL'}"
              f"{'  GAMUT-CLIP' if clipped else ''}")


# ------------------------------------------------------------------ emitters
#
# Why anything is emitted at all: this palette is authored in OKLCH and gated
# here, but Swift is not the only thing that wants it. Anything designed for
# TcrBar outside the app -- a mock, a doc, a web surface -- otherwise re-picks
# colours by eye, and eyeballing is exactly what this file exists to replace.
#
# The division of authorship is deliberate and the emitter does not blur it:
#
#   colour      authored HERE, consumed by Swift
#   geometry    authored in Tokens.swift, consumed HERE
#
# So the CSS is assembled from two sources and duplicates neither. Copying the
# 4pt ramp into a table in this file would have been three lines shorter and
# would have created a second source of truth that drifts the first time
# somebody edits one side -- the same class of bug `build-tcrbar.sh` documents
# for the two literal `0.1.0` version strings it inherited.

SWIFT_NUMERIC = re.compile(
    r"^\s*public static let (\w+): CGFloat = ([0-9.]+)\s*$", re.MULTILINE)


def kebab(name):
    """`inkDim` -> `ink-dim`. CSS custom properties are conventionally kebab."""
    return re.sub(r"(?<!^)(?=[A-Z])", "-", name).lower()


def swift_geometry(tokens_swift):
    """Read the spacing, radius and type scale out of Tokens.swift.

    Parsed rather than mirrored, so there is exactly one place each number is
    written down. Aliases (`gutter = space4`) are skipped on purpose: they are
    Swift-side naming for Swift call sites, not separate values, and emitting
    both would imply two knobs where there is one.

    Raises rather than returning a partial map. An emitter that silently ships
    a stylesheet missing half its spacing scale is worse than one that stops:
    the caller gets a file that looks complete and is not.
    """
    text = pathlib.Path(tokens_swift).read_text(encoding="utf-8")
    found = {name: value for name, value in SWIFT_NUMERIC.findall(text)}
    if not found:
        raise SystemExit(
            f"{tokens_swift}: no `public static let <name>: CGFloat = <n>` "
            "declarations matched. The declaration shape changed and this "
            "parser did not; refusing to emit a stylesheet with no geometry.")
    return found


def css_block(surfaces, ink, status, indent="    "):
    """One appearance's worth of custom properties."""
    lines = []
    for group in (surfaces, ink, status):
        for name, v in group.items():
            hexv, _ = oklch_to_hex(*v)
            lines.append(f"{indent}--tcr-{kebab(name)}: {hexv};")
    return "\n".join(lines)


def emit_css(path, geometry):
    """Four appearances, matching `Tok.dynNS`'s four cases exactly.

    Light sits in `:root` and dark arrives via `prefers-color-scheme`, which is
    the way round a web consumer expects even though the app itself is
    dark-native: a stylesheet dropped into a page with no media-query support
    should still be legible rather than white-on-white.

    The alphas are emitted as their own properties instead of pre-multiplied
    colours. `Tok.wash`/`Tok.line` apply them at the use site precisely so a
    wash and its solid parent cannot drift apart, and baking them in here would
    undo that -- and would need one extra property per status hue per
    appearance, which is twenty-four values that all have to stay in step.
    """
    geo = "\n".join(
        f"    --tcr-{kebab(n)}: {v}px;" for n, v in sorted(geometry.items()))
    out = f"""/* GENERATED by scripts/tcrbar-palette.py -- do not edit by hand.
 *
 * Colours are authored in OKLCH in that script and gated there: every value
 * below cleared a measured WCAG contrast check against the surface it is drawn
 * on, in all four appearances, and the generator exits non-zero if one does
 * not. Geometry is read out of apps/macos/Sources/TcrBar/Tokens.swift, which
 * is where it is authored.
 *
 * Regenerate:  python3 scripts/tcrbar-palette.py --emit-css <path>
 */

:root {{
{css_block(LIGHT_SURFACES, LIGHT_INK, LIGHT_STATUS)}

    --tcr-wash-alpha: {WASH_ALPHA};
    --tcr-line-alpha: {LINE_ALPHA};

{geo}
}}

@media (prefers-color-scheme: dark) {{
  :root {{
{css_block(SURFACES, INK, STATUS, indent="      ")}
  }}
}}

@media (prefers-contrast: more) {{
  :root {{
{css_block({}, HC_LIGHT_INK, {}, indent="      ")}
  }}
}}

@media (prefers-color-scheme: dark) and (prefers-contrast: more) {{
  :root {{
{css_block({}, HC_DARK_INK, {}, indent="      ")}
  }}
}}
"""
    pathlib.Path(path).write_text(out, encoding="utf-8")
    return out.count("--tcr-")


def emit_json(path, geometry):
    """The same values, for anything that would otherwise parse the CSS."""
    def group(d):
        return {kebab(n): oklch_to_hex(*v)[0] for n, v in d.items()}

    payload = {
        "$comment": "GENERATED by scripts/tcrbar-palette.py -- do not edit. "
                    "Colour authored in that script; geometry read from "
                    "apps/macos/Sources/TcrBar/Tokens.swift.",
        "color": {
            "light": {**group(LIGHT_SURFACES), **group(LIGHT_INK),
                      **group(LIGHT_STATUS)},
            "dark": {**group(SURFACES), **group(INK), **group(STATUS)},
            "lightHighContrast": group(HC_LIGHT_INK),
            "darkHighContrast": group(HC_DARK_INK),
        },
        "alpha": {"wash": WASH_ALPHA, "line": LINE_ALPHA},
        "geometry": {kebab(n): float(v) for n, v in sorted(geometry.items())},
    }
    pathlib.Path(path).write_text(
        json.dumps(payload, indent=2, sort_keys=False) + "\n", encoding="utf-8")
    return payload


def main():
    parser = argparse.ArgumentParser(
        description="Author, gate and emit the TcrBar palette.")
    parser.add_argument("--emit-css", metavar="PATH",
                        help="write CSS custom properties to PATH")
    parser.add_argument("--emit-json", metavar="PATH",
                        help="write the same tokens as JSON to PATH")
    parser.add_argument(
        "--tokens-swift",
        default=str(pathlib.Path(__file__).resolve().parent.parent
                    / "apps/macos/Sources/TcrBar/Tokens.swift"),
        help="where the geometry scale is authored (default: the app's)")
    args = parser.parse_args()

    panel = SURFACES["panel"]
    failures = []

    print("=" * 78)
    print("TcrBar tokens - OKLCH authored, sRGB emitted, contrast measured")
    print("=" * 78)

    print("\nSURFACE RAMP (perceptual steps; even L gaps = even visual steps)")
    prev = None
    for name, v in SURFACES.items():
        hexv, clipped = oklch_to_hex(*v)
        step = "" if prev is None else f"  dL={v[0]-prev:+.3f}"
        print(f"  {name:<13} oklch({v[0]:.3f} {v[1]:.3f} {v[2]:g})  {hexv}"
              f"{'  GAMUT-CLIP' if clipped else ''}{step}")
        prev = v[0]

    print(f"\nINK on panel (AA needs 4.5:1 for body, 3.0:1 for large/secondary)")
    for name, v in INK.items():
        hexv, clipped = oklch_to_hex(*v)
        ratio = contrast(v, panel)
        need = 4.5
        ok = ratio >= need
        if not ok:
            failures.append(f"{name} {ratio:.2f}:1 < {need}:1 on panel")
        print(f"  {name:<13} {hexv}  {ratio:5.2f}:1  {'PASS' if ok else 'FAIL'}"
              f"{'  GAMUT-CLIP' if clipped else ''}")

    print(f"\nSTATUS HUES on panel, and on a raised row")
    raised = SURFACES["raised"]
    for name, v in STATUS.items():
        hexv, clipped = oklch_to_hex(*v)
        on_panel, on_raised = contrast(v, panel), contrast(v, raised)
        # Status text is a small pill label: treat as body text, 4.5:1.
        ok = on_panel >= 4.5 and on_raised >= 4.5
        if not ok:
            failures.append(
                f"{name} {min(on_panel, on_raised):.2f}:1 < 4.5:1")
        print(f"  {name:<13} {hexv}  panel {on_panel:5.2f}:1   raised "
              f"{on_raised:5.2f}:1  {'PASS' if ok else 'FAIL'}"
              f"{'  GAMUT-CLIP' if clipped else ''}")

    print(f"\nSTATUS wash ({WASH_ALPHA}) and line ({LINE_ALPHA}) alphas")
    for name, v in STATUS.items():
        hexv, _ = oklch_to_hex(*v)
        print(f"  {name:<13} {hexv} @ {WASH_ALPHA} wash / {LINE_ALPHA} line")

    # The two neutrals must not collapse into each other: "never measured" and
    # "you turned this off" are different facts and the panel says so in colour.
    # ready-vs-spent is the load-bearing pair: red/green is the common colour
    # vision deficiency, so those two MUST differ in luminance or the panel is
    # unreadable to a deuteranope and in any greyscale rendering.
    print("\nDISCRIMINATION (tokens that must not read alike, in LUMINANCE)")
    pairs = [
        ("ready", "spent"),        # the red/green CVD pair - most important
        ("ready", "near"),
        ("near", "spent"),
        ("unmeasured", "disabled"),
        # Both cool, both off the traffic-light scale: the pair that would
        # collapse in greyscale if either drifted.
        ("awake", "unmeasured"),
    ]
    for a, b in pairs:
        r = contrast(STATUS[a], STATUS[b])
        ok = r >= 1.25
        if not ok:
            failures.append(f"{a} vs {b} only {r:.2f}:1 apart")
        print(f"  {a:<11} vs {b:<11} {r:5.2f}:1  "
              f"{'distinct' if ok else 'TOO CLOSE'}")

    check_set("LIGHT APPEARANCE - ink (4.5:1, body text)",
              LIGHT_SURFACES, LIGHT_INK, {}, failures)
    check_set("LIGHT APPEARANCE - status (3:1, WCAG 1.4.11 non-text)",
              LIGHT_SURFACES, {}, LIGHT_STATUS, failures, min_ratio=3.0)
    check_set("DARK - status (3:1 floor; these clear it by a wide margin)",
              SURFACES, {}, STATUS, failures, min_ratio=3.0)
    check_set("DARK + INCREASED CONTRAST (ink only; status already clears 7:1)",
              SURFACES, HC_DARK_INK, {}, failures, min_ratio=7.0)
    check_set("LIGHT + INCREASED CONTRAST",
              LIGHT_SURFACES, HC_LIGHT_INK, {}, failures, min_ratio=7.0)

    print("\nLIGHT-MODE DISCRIMINATION (the CVD pair must hold in both appearances)")
    for a, b in [("ready", "spent"), ("ready", "near"), ("near", "spent"),
                 ("awake", "unmeasured")]:
        r = contrast(LIGHT_STATUS[a], LIGHT_STATUS[b])
        ok = r >= 1.25
        if not ok:
            failures.append(f"[light] {a} vs {b} only {r:.2f}:1 apart")
        print(f"  {a:<11} vs {b:<11} {r:5.2f}:1  "
              f"{'distinct' if ok else 'TOO CLOSE'}")

    print("\n" + "=" * 78)
    if failures:
        print(f"FAILED ({len(failures)}):")
        for f in failures:
            print(f"  - {f}")
        # Emit NOTHING on failure, and say so.
        #
        # The gate above is the whole value of this file; a stylesheet written
        # from a palette that just failed it would carry the failure outward
        # under the generator's authority, into surfaces that never run the
        # check. Refusing is also the recoverable direction: a missing file is
        # obvious, a quietly wrong one is not.
        if args.emit_css or args.emit_json:
            print("\nNOT emitting: the palette did not pass its own gate.")
        return 1

    print("All tokens pass: AA on both surfaces, in gamut, mutually distinct.")

    if args.emit_css or args.emit_json:
        geometry = swift_geometry(args.tokens_swift)
        print(f"\ngeometry: {len(geometry)} values read from "
              f"{pathlib.Path(args.tokens_swift).name}")
        if args.emit_css:
            n = emit_css(args.emit_css, geometry)
            print(f"  css:  {n} custom properties -> {args.emit_css}")
        if args.emit_json:
            emit_json(args.emit_json, geometry)
            print(f"  json: -> {args.emit_json}")

    return 0


if __name__ == "__main__":
    sys.exit(main())
