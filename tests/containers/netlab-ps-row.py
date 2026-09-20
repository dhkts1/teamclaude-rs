#!/usr/bin/env python3
"""Read `tcr status --json`'s bare array off stdin, print one line: `<source>
<status> <account>` for its first row, or `- - -` when the array is empty or
the input did not parse.

`netlab ps` shells out to this rather than grepping the JSON by hand: a status
row is an object, and a field pulled out with grep is one rename away from
silently reading the wrong key.
"""

import json
import sys

try:
    rows = json.load(sys.stdin)
except Exception:
    rows = []

if rows:
    row = rows[0]
    print(f"{row.get('source', '-')} {row.get('status', '-')} {row.get('name', '-')}")
else:
    print("- - -")
