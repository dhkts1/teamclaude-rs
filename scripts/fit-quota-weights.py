#!/usr/bin/env python3 -I
"""Fit how each token kind weighs against the 5h window, from real traffic.

Joins the proxy log's `quota-sample` lines (one per successful usage probe,
written by Manager::apply_usage) with the usage ledger
(~/.cache/teamclaude/usage/*.jsonl: t, a, i, c5, c1, r, o). For every pair of
consecutive samples of one account inside one 5h window (same reset instant)
whose utilization ROSE, the rise is the dependent variable and the ledger tokens
served on that account between the two samples are the regressors:

    d_util = w_i*i + w_c5*c5 + w_c1*c1 + w_r*r + w_o*o

solved by ordinary least squares (normal equations, no numpy). The ratio
w_c1 / w_c5 is the question this exists to answer: does a 1h cache write cost
the quota 2x a 5m write (API price ratio 2.0/1.25 = 1.6) or the same.

Because both instruments report whole percents, a single interval carries
quantisation noise of up to one tick; the fit needs a day of samples, and the
script prints how many intervals it used and the residual so a thin fit reads as
thin. Nothing here spends quota.

usage: python3 -I scripts/fit-quota-weights.py <proxy.log>... --ledger <jsonl>...
"""

import json
import re
import sys
from datetime import datetime, timezone

SAMPLE = re.compile(
    r"^(?P<ts>\S+)\s+INFO\s+\S+:\s+quota-sample: usage probe read"
    r'\s+account="?(?P<account>[^"\s]+)"?'
    r"\s+five_hour=(?P<five>-?[0-9.]+)"
    r"\s+five_hour_reset_ms=(?P<reset>-?\d+)"
)
KINDS = ["i", "c5", "c1", "r", "o"]


def parse_ts(s):
    return datetime.fromisoformat(s.replace("Z", "+00:00")).timestamp() * 1000


def read_samples(paths):
    out = {}
    for path in paths:
        with open(path, errors="replace") as f:
            for line in f:
                m = SAMPLE.search(line)
                if not m:
                    continue
                five = float(m["five"])
                if five < 0:
                    continue
                out.setdefault(m["account"], []).append(
                    (parse_ts(m["ts"]), five, int(m["reset"]))
                )
    for rows in out.values():
        rows.sort()
    return out


def read_ledger(paths):
    rows = []
    for path in paths:
        with open(path) as f:
            for line in f:
                row = json.loads(line)
                rows.append(row)
    rows.sort(key=lambda r: r["t"])
    return rows


def intervals(samples, ledger):
    """(d_util, tokens-by-kind) per rising step inside one window."""
    out = []
    for account, rows in samples.items():
        acct_rows = [r for r in ledger if r["a"].startswith(account)]
        for (t0, u0, reset0), (t1, u1, reset1) in zip(rows, rows[1:]):
            if reset0 != reset1 or u1 <= u0:
                continue
            toks = {k: 0 for k in KINDS}
            for r in acct_rows:
                if t0 < r["t"] <= t1:
                    for k in KINDS:
                        toks[k] += r.get(k, 0)
            if sum(toks.values()) == 0:
                continue
            out.append((u1 - u0, [toks[k] for k in KINDS]))
    return out


def solve_normal_equations(xs, ys):
    n = len(KINDS)
    ata = [[0.0] * n for _ in range(n)]
    aty = [0.0] * n
    for x, y in zip(xs, ys):
        for a in range(n):
            aty[a] += x[a] * y
            for b in range(n):
                ata[a][b] += x[a] * x[b]
    # Gauss-Jordan with partial pivoting.
    m = [ata[i] + [aty[i]] for i in range(n)]
    for col in range(n):
        pivot = max(range(col, n), key=lambda r: abs(m[r][col]))
        if abs(m[pivot][col]) < 1e-18:
            return None
        m[col], m[pivot] = m[pivot], m[col]
        p = m[col][col]
        m[col] = [v / p for v in m[col]]
        for r in range(n):
            if r != col and m[r][col] != 0.0:
                f = m[r][col]
                m[r] = [rv - f * cv for rv, cv in zip(m[r], m[col])]
    return [m[i][n] for i in range(n)]


def main():
    argv = sys.argv[1:]
    if "--ledger" not in argv:
        sys.exit(__doc__)
    split = argv.index("--ledger")
    logs, ledgers = argv[:split], argv[split + 1 :]
    samples = read_samples(logs)
    ledger = read_ledger(ledgers)
    ivs = intervals(samples, ledger)
    print(f"accounts={len(samples)} samples={sum(len(v) for v in samples.values())} "
          f"rising_intervals={len(ivs)}")
    if len(ivs) < 2 * len(KINDS):
        sys.exit("too few rising intervals for a fit; collect more samples")
    xs = [x for _, x in ivs]
    ys = [y for y, _ in ivs]
    w = solve_normal_equations(xs, ys)
    if w is None:
        sys.exit("singular system: a token kind never varied across intervals")
    # Weight per million tokens, as a share of the window.
    for k, wk in zip(KINDS, w):
        print(f"  w_{k}={wk * 1e6:.5f} window-fraction per Mtok")
    if w[1] > 0:
        print(f"  c1/c5 ratio={w[2] / w[1]:.3f}  (API price ratio 1.600)")
    resid = sum((y - sum(wk * xk for wk, xk in zip(w, x))) ** 2 for x, y in zip(xs, ys))
    print(f"  residual_sum_sq={resid:.6f} over {len(ivs)} intervals")


if __name__ == "__main__":
    main()
