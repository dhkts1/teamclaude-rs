#!/usr/bin/env bash
# check-public-disclosure.sh — the internal-scaffolding half of the disclosure
# scan, run over every TRACKED file, or over a PR's title and body.
#
# WHY THIS EXISTS. `.githooks/pre-commit` (via `.githooks/lib/disclosure-scan.sh`)
# catches a new citation of an internal planning path, or internal process
# vocabulary (the token list lives in that library), the moment it is staged
# — but it protects only commits made ON THIS MACHINE, through this
# checkout's hooks. This repository takes pull requests from forks, and a
# fork's contributor never runs our hooks: their branch, or their PR's own
# title and body, can carry the exact same dangling internal-only citation
# straight past every local gate. This script is the same check, meant to
# run in CI, where a fork has to pass it too.
#
# It shares its pattern with `.githooks/lib/disclosure-scan.sh`'s check 4 via
# `TCR_SCAFFOLDING_PATTERN`, sourced from that same library — one list, not
# two that drift apart.
#
# `.githooks/**` is excluded from the tracked-tree scan for the same reason
# the library excludes it from the staged-diff scan: this script's own
# source, and the hooks that call its sibling library, have to describe the
# tokens they block in prose.
#
# Output is `file:line: error: internal-scaffolding reference: <text>`, one
# line per hit, greppable and intuitive at a glance; exits 1 on any hit.
#
# Usage:
#   scripts/check-public-disclosure.sh                    # scan every tracked file
#   scripts/check-public-disclosure.sh --stdin <label>     # scan stdin (a PR title or body)
set -uo pipefail
# Not `-e`: `git grep` and `grep` both exit 1 on "no match", which is the
# CLEAN result here, not an error to abort on.

repo_root="$(git rev-parse --show-toplevel)" || {
  echo "check-public-disclosure: BLOCKED — not inside a git repository." >&2
  exit 1
}
cd "$repo_root" || exit 1

lib=".githooks/lib/disclosure-scan.sh"
if [ ! -f "$lib" ]; then
  echo "check-public-disclosure: BLOCKED — $lib is missing; the checkout is damaged." >&2
  exit 1
fi
# shellcheck source=.githooks/lib/disclosure-scan.sh
. "$lib"

scan_stdin() {
  local label="$1" text found
  text="$(cat)"
  if [ -z "$text" ]; then
    echo "check-public-disclosure: ${label} clean (empty)."
    return 0
  fi
  found="$(printf '%s\n' "$text" | grep -n -E -i "$TCR_SCAFFOLDING_PATTERN" || true)"
  if [ -n "$found" ]; then
    printf '%s\n' "$found" | while IFS=: read -r lineno content; do
      echo "${label}:${lineno}: error: internal-scaffolding reference:${content}"
    done
    echo ""
    echo "check-public-disclosure: BLOCKED — ${label} references internal (Henry)"
    echo "  scaffolding. This repository is public; fix the text above."
    return 1
  fi
  echo "check-public-disclosure: ${label} clean."
  return 0
}

if [ "${1:-}" = "--stdin" ]; then
  scan_stdin "${2:-input}"
  exit $?
fi

found="$(git grep -n -I -i -E "$TCR_SCAFFOLDING_PATTERN" -- . ':(exclude).githooks/**' 2>/dev/null || true)"
if [ -n "$found" ]; then
  printf '%s\n' "$found" | while IFS=: read -r file lineno content; do
    echo "${file}:${lineno}: error: internal-scaffolding reference:${content}"
  done
  echo ""
  echo "check-public-disclosure: BLOCKED — the tracked tree references internal (Henry)"
  echo "  scaffolding. This repository is public; fix the lines above."
  exit 1
fi

echo "check-public-disclosure: tracked tree clean."
exit 0
