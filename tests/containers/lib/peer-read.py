#!/usr/bin/env python3
"""Read one fact out of `tcr peer ls --json`, on stdin, as plain lines a shell loops over.

The scenarios are `sh`, the listing is JSON, and the assertions are about a
field several levels in. This prints the field and decides nothing: every
PASS/FAIL line in this harness is printed by `lib/assert.sh`, so the verdict
stays in the scenario where a reader looks for it.

Stdlib only, host-side, the same choice `mdns-observe.py` and `upstream-stub.py`
make in their containers.

    nodes                  every pinned row's node id, one per line
    labels                 every pinned row as `<node-id> <label>`
    endpoints <node-id>    that row's endpoints, one per line, as either
                           `direct <host:port>` or `<kind> <node-id>`

Exit 0 when the listing parsed, 2 when it did not, and 1 when the row asked for
is not in it: a scenario that cannot tell "no such row" from "a row with no
endpoints" would read a missing pin as a clean absence of bad endpoints.
"""

import json
import sys


def main() -> int:
    argv = sys.argv[1:]
    if not argv:
        print("peer-read: needs one of nodes, labels, endpoints", file=sys.stderr)
        return 2
    raw = sys.stdin.read()
    try:
        listing = json.loads(raw)
    except json.JSONDecodeError as err:
        print(f"peer-read: stdin is not `tcr peer ls --json` output: {err}", file=sys.stderr)
        print(raw[:400], file=sys.stderr)
        return 2

    rows = listing.get("peers") or []
    what = argv[0]

    if what == "nodes":
        for row in rows:
            print(row.get("node", ""))
        return 0

    if what == "labels":
        for row in rows:
            print(f"{row.get('node', '')} {row.get('label', '')}")
        return 0

    if what == "endpoints":
        if len(argv) < 2:
            print("peer-read: endpoints needs the node id to read", file=sys.stderr)
            return 2
        wanted = argv[1]
        for row in rows:
            if row.get("node") != wanted:
                continue
            for endpoint in row.get("endpoints") or []:
                kind = endpoint.get("kind", "?")
                if kind == "direct":
                    print(f"direct {endpoint.get('addr', '')}")
                else:
                    print(f"{kind} {endpoint.get('node', '')}")
            return 0
        print(f"peer-read: no pinned row for {wanted}", file=sys.stderr)
        return 1

    print(f"peer-read: no reader named {what}", file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main())
