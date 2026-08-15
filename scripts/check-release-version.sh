#!/usr/bin/env bash
# check-release-version.sh — refuse a diff that ships code under a version
# that has ALREADY been released.
#
# Extracted from .githooks/pre-commit's release-version gate (see that file
# for the full incident writeup, 2026-08-09). That hook can only ever see the
# tags that exist AT COMMIT TIME — the tag that actually invalidates a branch
# is created later, at release. Two branches can each bump to the same next
# version, both pass the commit-time gate legitimately, and merge with no
# conflict at all (same line, same value) — nothing for a human to notice.
# This script is the second caller: run again at PR/merge time, against a
# fully-fetched tag list, it catches the case the commit-time hook structurally
# cannot.
#
# Usage:
#   check-release-version.sh <diff-filter-args...>
#
# The caller supplies how to enumerate the shippable file list — `git diff
# --cached --name-only --diff-filter=ACMR -- .` for the pre-commit hook,
# `git diff --name-only --diff-filter=ACMR <base>...<head> -- .` for CI —
# because "which files changed" differs by context but "which files are
# shippable" and "what counts as already released" do not. Everything after
# the caller's own diff invocation is the same pathspec exclusion list, so
# this script takes the fully-formed list of changed files on stdin (one per
# line) and applies the shippable-file exclusion + version-already-tagged
# check uniformly.
#
# Reads the version to check from `git show :Cargo.toml` in pre-commit
# context (the INDEX) or from the working tree in CI context — see
# --source below.
#
# Exit 0: no shippable changes, or version not yet released, or version
#         could not be read (SKIPPED, matching the pre-commit hook's existing
#         graceful-degradation behaviour).
# Exit 1: the version being shipped is already a released tag.
set -euo pipefail

usage() {
  cat >&2 <<'EOF'
Usage: check-release-version.sh --source=<index|worktree> [changed-file...]

  --source=index      Read Cargo.toml from the git index (`git show :Cargo.toml`).
                       Use this from a pre-commit hook.
  --source=worktree    Read Cargo.toml from the working tree. Use this from CI,
                       where there is no index to speak of — just a checkout.

Changed files are passed as positional arguments (already diff-filtered by
the caller); this script applies the shippable-file exclusion list and the
already-released-tag check.
EOF
}

source_mode=""
args=()
for arg in "$@"; do
  case "$arg" in
    --source=*)
      source_mode="${arg#--source=}"
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      args+=("$arg")
      ;;
  esac
done

if [ "$source_mode" != "index" ] && [ "$source_mode" != "worktree" ]; then
  echo "check-release-version.sh: --source must be 'index' or 'worktree'" >&2
  usage
  exit 2
fi

# Shippable-file exclusion list — authoritative copy from
# .githooks/pre-commit:279-286. Do not re-derive or "tidy" this; every entry
# has a documented reason in that file.
shippable=()
for f in "${args[@]}"; do
  case "$f" in
    *.md) continue ;;
    docs/*) continue ;;
    assets/*) continue ;;
    .github/*) continue ;;
    .githooks/*) continue ;;
    apps/macos/appcast.xml) continue ;;
    apps/macos/scripts/release-preflight.sh) continue ;;
    *) shippable+=("$f") ;;
  esac
done

if [ "${#shippable[@]}" -eq 0 ]; then
  exit 0
fi

# The sed is character-for-character the one in .githooks/pre-commit:297-299
# and build-tcrbar.sh:98. A gate that reads a different number than the thing
# it guards is worse than no gate.
if [ "$source_mode" = "index" ]; then
  version="$(git show :Cargo.toml 2>/dev/null \
            | sed -n 's/^version[[:space:]]*=[[:space:]]*"\([0-9][^"]*\)".*/\1/p' \
            | head -1 || true)"
else
  version="$(sed -n 's/^version[[:space:]]*=[[:space:]]*"\([0-9][^"]*\)".*/\1/p' Cargo.toml \
            | head -1 || true)"
fi

if [ -z "$version" ]; then
  echo "check-release-version.sh: could not read a version from Cargo.toml — release-version gate SKIPPED." >&2
  exit 0
fi

if git rev-parse -q --verify "refs/tags/v$version" >/dev/null 2>&1; then
  echo "" >&2
  echo "check-release-version.sh: BLOCKED — v$version is already a released tag." >&2
  echo "" >&2
  echo "  This change ships shippable files but leaves Cargo.toml at $version," >&2
  echo "  a version that has already been tagged and published. Everything built" >&2
  echo "  from here would claim to be $version while not being it." >&2
  echo "" >&2
  echo "  Bump the version in Cargo.toml to the next one." >&2
  echo "" >&2
  echo "  Shippable files that triggered this:" >&2
  printf '      %s\n' "${shippable[@]}" >&2
  echo "" >&2
  exit 1
fi

exit 0
