#!/usr/bin/env python3
"""Print the SHAPE of the live fleet: counts and distributions, never identities.

Usage: scripts/fleet-shape.py [--url URL] [--from FILE]

Why this exists.

Every screen of the TcrBar panel was designed and reviewed against the fixtures in
`apps/macos/Sources/TcrBar/RenderStates.swift`. Those fixtures are careful and
internally consistent -- the headline equals the sum of `byTool`, the timeout card's
total equals the number the summary line prints -- and they describe a fleet that does
not exist. Measured 2026-09-17 against the live proxy, the two disagreed like this:

    dimension          fixture     live
    running calls            3       57   (51 of them ghosts that never close)
    sessions                 5       28   (14 of them warm-ups: 2 requests, no tools)
    accounts                13       18
    timeouts today          31        0   (so TIMED OUT TODAY renders only in fixtures)

Nobody had looked at the panel's real appearance, because the only reader the design
ever had was the fixture. This script is the second reader. Run it, compare its numbers
against the scene you are about to add or change, and you will notice the drift while it
is still cheap.

What it deliberately does NOT print, so its output is safe to paste into a public issue,
a commit message or a PR review on this public repository: account emails, organization
and account UUIDs, session ids, workspace and project names, and command text. Only
counts, bucket labels and durations leave this script. `--self-check` asserts that.
"""

import argparse
import json
import re
import sys
import math
import time
import urllib.request
from collections import Counter

DEFAULT_URL = "http://127.0.0.1:3456/_tcr/status"

# A warm-up is a session that made at most this many requests, ran no tool at all, and
# has since gone quiet for at least this long. All three clauses matter: without the
# idle clause the live count on 2026-09-17 went from 14 to 17, and the extra three were
# sessions that had only just started -- the moment an operator most wants to see them.
WARMUP_MAX_REQUESTS = 2
WARMUP_MIN_IDLE_S = 5 * 60

# Tools this build knows a timeout for. Only such a call can be "about to time out",
# which is the Tools tab's first question; everything else is uncapped work.
CAPPED_TOOLS = {"Bash"}


def load(url, path):
    if path:
        with open(path) as handle:
            return json.load(handle)
    with urllib.request.urlopen(url, timeout=10) as response:
        return json.loads(response.read())


def shape(doc, now_ms):
    accounts = doc.get("accounts") or []
    sessions = doc.get("sessions") or []
    summary = doc.get("sessions_summary") or {}

    running = [
        call
        for session in sessions
        for call in ((session.get("tools") or {}).get("running") or [])
    ]

    def idle_s(session):
        return (now_ms - session.get("lastSeenMs", now_ms)) / 1000

    def is_warmup(session):
        tools = session.get("tools") or {}
        return (
            session.get("requests", 0) <= WARMUP_MAX_REQUESTS
            and (tools.get("calls") or 0) == 0
            and idle_s(session) > WARMUP_MIN_IDLE_S
        )

    warmups = [s for s in sessions if is_warmup(s)]
    ages = sorted((now_ms - c.get("startedMs", now_ms)) / 1000 for c in running)
    idles = sorted(idle_s(s) for s in sessions)

    out = []
    add = out.append
    add(("accounts.total", len(accounts)))
    for state, count in sorted(Counter(a.get("quotaState") for a in accounts).items()):
        add((f"accounts.quotaState.{state}", count))
    add(("accounts.disabled", sum(1 for a in accounts if a.get("disabled"))))
    add(("accounts.grouped", sum(1 for a in accounts if a.get("groups"))))

    add(("sessions.total", len(sessions)))
    add(("sessions.warmups", len(warmups)))
    add(("sessions.warmups.costUsd", round(sum(s.get("costUsd", 0) for s in warmups), 2)))
    add(("sessions.visible", len(sessions) - len(warmups)))
    if idles:
        add(("sessions.idleSeconds.median", int(idles[len(idles) // 2])))
        add(("sessions.idleSeconds.max", int(idles[-1])))

    add(("running.total", len(running)))
    for tool, count in sorted(Counter(c.get("tool") for c in running).items()):
        add((f"running.byTool.{tool}", count))
    add(("running.capped", sum(1 for c in running if c.get("tool") in CAPPED_TOOLS)))
    add(("running.uncapped", sum(1 for c in running if c.get("tool") not in CAPPED_TOOLS)))
    if ages:
        add(("running.ageSeconds.min", int(ages[0])))
        add(("running.ageSeconds.max", int(ages[-1])))

    add(("today.calls", summary.get("calls", 0)))
    add(("today.overOneMinute", summary.get("overOneMinute", 0)))
    add(("today.timeouts", summary.get("timeouts", 0)))
    for name, count in sorted((summary.get("timeoutsByClass") or {}).items()):
        add((f"today.timeoutsByClass.{name}", count))
    add(("today.timeoutCardVisible", int(bool(summary.get("timeoutsByClass")))))
    return out


# An email, a UUID, or an absolute path. If any of these reaches the output, the
# no-identities promise in this script's docstring is broken and it must not be run
# against a public surface. The check is a positive control, not decoration: it is
# handed a string it MUST reject before it is trusted on the real output.
LEAK = re.compile(
    r"[\w.+-]+@[\w-]+\.[\w.]+"
    r"|[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}"
    r"|/Users/[^\s]+"
)


def self_check():
    poison = "user@example.com 11111111-1111-1111-1111-111111111111 /Users/alice/git"
    found = LEAK.findall(poison)
    if len(found) != 3:
        print(f"fleet-shape: self-check FAILED, detector missed a leak: {found}", file=sys.stderr)
        return 1
    print("fleet-shape: self-check ok (leak detector rejects email, uuid and path)")
    return 0


def coarsen(value):
    """Round a figure to two significant digits before it is ever printed.

    This script exists to report the SHAPE of a fleet, and a shape is a
    magnitude: whether a panel renders correctly at 1,600 requests does not
    depend on the number being 1,654. Exact values are the operator's own
    business volume and spend.

    The original version guarded its output with a regex for emails, UUIDs and
    absolute paths, and had a self-check proving that regex fires. Not one of
    those looks at a number, so on 2026-09-17 the figures this script prints,
    and per-session values read past it by hand, went into a public repository
    wearing fake names. A detector only ever catches the class you thought of.
    Rounding makes the precise value impossible to emit instead of detectable,
    which is the difference between a guard and a property.

    Rounds toward the nearest, never truncates, so a magnitude stays honest.
    """
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return value
    if value == 0:
        return 0
    magnitude = math.floor(math.log10(abs(value)))
    step = 10 ** (magnitude - 1)
    rounded = round(value / step) * step
    return int(rounded) if isinstance(value, int) or float(rounded).is_integer() else round(rounded, 2)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--url", default=DEFAULT_URL)
    parser.add_argument("--from", dest="path", help="read a saved /_tcr/status body instead")
    parser.add_argument("--self-check", action="store_true", help="prove the leak detector fires, then exit")
    args = parser.parse_args()

    if args.self_check:
        return self_check()
    if self_check() != 0:
        return 1

    try:
        doc = load(args.url, args.path)
    except Exception as error:  # noqa: BLE001 - the reason is what the operator needs
        print(f"fleet-shape: could not read the fleet: {error}", file=sys.stderr)
        print("fleet-shape: is the proxy running? try: tcr status", file=sys.stderr)
        return 2

    lines = [f"{key}: {coarsen(value)}" for key, value in shape(doc, time.time() * 1000)]
    leaked = [line for line in lines if LEAK.search(line)]
    if leaked:
        print(f"fleet-shape: REFUSING to print, an identity reached the output: {leaked[0]}", file=sys.stderr)
        return 3
    print("\n".join(lines))
    return 0


if __name__ == "__main__":
    sys.exit(main())
