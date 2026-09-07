#!/usr/bin/env bash
# check-no-org-flag.sh — refuse any code that builds or declares an `--org`
# flag on a `tcr` account verb.
#
# `--org` existed to pick one of two rows sharing a name, back when
# `Account.name` was an email and one email could name two accounts. Names are
# unique now (`src/config.rs`, `migrate_duplicate_names`), so the flag narrows
# nothing — and a flag in `--help` that narrows nothing is a lie the next reader
# has to disprove. It came back once already, one verb at a time, which is why
# this is a gate and not a note.
#
# It looks for the two ways the flag can exist, and nothing else:
#
#   1. The literal `"--org"` in an argument vector the panel hands to `tcr`.
#   2. A clap `org` argument on a CLI verb — the `#[arg(long)] org:` pair in
#      `src/main.rs`, which is what would put it back in `--help`.
#
# Test files are exempt from (1) BY NAME, because the panel's own gate
# (`AccountIdentityTests.testNoCommandBuildsAnOrgFlag`) has to write the literal
# down in order to assert nothing builds it. The exemption is a fixed list, not
# a pattern: a new test file does not silently inherit it.
#
# Usage: scripts/check-no-org-flag.sh [root]   (default: the repo root)

set -euo pipefail

root="${1:-$(git rev-parse --show-toplevel)}"

# The one file allowed to write the literal: it is the assertion that nothing
# builds it.
exempt="apps/macos/Tests/TcrBarTests/AccountIdentityTests.swift"

status=0

# (1) The flag as a built argument.
literals=$(
    grep -rn -- '"--org"' "$root/src" "$root/apps" "$root/crates" 2>/dev/null |
        grep -v "/$exempt:" || true
)
if [ -n "$literals" ]; then
    echo "error: an --org flag is built here, and tcr no longer takes one:" >&2
    echo "$literals" >&2
    status=1
fi

# (2) The flag as a clap argument. Matches the `#[arg(long)]` line and the `org:`
# field on the line after it, which is the shape every account verb used.
declarations=$(
    grep -rn -A1 -- '#\[arg(long' "$root/src" 2>/dev/null |
        grep -E '^\S+[-:][0-9]+[-:]\s*org: Option<String>,' || true
)
if [ -n "$declarations" ]; then
    echo "error: an --org clap argument is declared here, and would show in --help:" >&2
    echo "$declarations" >&2
    status=1
fi

if [ "$status" -eq 0 ]; then
    echo "check-no-org-flag: clean (no --org flag is built or declared)"
fi
exit "$status"
