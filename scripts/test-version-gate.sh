#!/usr/bin/env bash
# Fixture test for the pre-commit release-version gate.
#
# A gate that has never been watched failing is not a gate. This runs the hook
# against a staged index in three states and asserts the exit code each time,
# including the negative controls -- a gate that blocks everything passes a
# block-test for the wrong reason.
#
# Safe to run: it stages and resets in THIS worktree's index only, never commits,
# and restores Cargo.toml at the end.
set -uo pipefail

cd "$(git rev-parse --show-toplevel)" || exit 1

hook=.githooks/pre-commit
original_version="$(sed -n 's/^version[[:space:]]*=[[:space:]]*"\([0-9][^"]*\)".*/\1/p' Cargo.toml | head -1)"
pass=0
fail=0

# Unstage FIRST, then restore from HEAD.
#
# `git checkout -- <file>` copies the INDEX over the worktree, so on a file
# this script has just staged it restores the staged modification rather than
# reverting it -- the test then leaves the tree dirty and the residue rides
# along in whatever someone commits next. Caught exactly that way: a stray
# newline on README.md blocked a rebase.
restore() {
  git reset -q 2>/dev/null
  git checkout HEAD -- Cargo.toml README.md 2>/dev/null
}
trap restore EXIT

# Run the hook and report only whether the VERSION gate fired. Other gates in
# the same script (gitleaks, disclosure, format) can also exit 1, and counting
# their failure as ours would be a false positive for this test.
check() {
  local label="$1" expect="$2" out rc
  out="$("$hook" 2>&1)"; rc=$?
  local fired=no
  case "$out" in *"is already a released tag"*) fired=yes ;; esac
  if [ "$fired" = "$expect" ]; then
    printf '  PASS  %-46s (gate fired=%s, exit=%s)\n' "$label" "$fired" "$rc"
    pass=$((pass + 1))
  else
    printf '  FAIL  %-46s (gate fired=%s, wanted=%s, exit=%s)\n' "$label" "$fired" "$expect" "$rc"
    printf '%s\n' "$out" | sed 's/^/        /'
    fail=$((fail + 1))
  fi
}

echo "release-version gate — fixture test"
echo "  Cargo.toml version: $original_version"
echo "  tag v$original_version exists: $(git rev-parse -q --verify "refs/tags/v$original_version" >/dev/null 2>&1 && echo yes || echo no)"
echo

# 1. POSITIVE CONTROL — shipped code staged at an already-released version.
#
# CONSTRUCT the condition; do not depend on the checked-in version happening
# to equal a tag. The first draft did, and it passed only because the tree was
# still sitting on the released version -- the moment the version was bumped
# (the normal state for a repo between releases) the control stopped being
# able to fire, and a control that cannot fire proves nothing about the gate.
released="$(git tag --list 'v*' --sort=-v:refname | head -1 | sed 's/^v//')"
if [ -z "$released" ]; then
  printf '  SKIP  %-46s (no v* tag exists to test against)\n' "code staged at released version -> BLOCK"
else
  git reset -q
  # ^version anchors to column 0, so this hits the [package] version and never
  # an indented dependency version.
  sed -i '' "s/^version = \".*\"/version = \"$released\"/" Cargo.toml
  git add Cargo.toml apps/macos/Sources/TcrBar/FleetView.swift 2>/dev/null
  check "code staged at released version -> BLOCK" yes
  git reset -q && git checkout HEAD -- Cargo.toml
fi

# 2. NEGATIVE CONTROL — same staged code, version bumped past the tag.
#    If this does not pass, the gate is blocking unconditionally and control 1
#    proved nothing.
sed -i '' "s/^version = \"$original_version\"/version = \"$original_version-gatetest\"/" Cargo.toml
git add Cargo.toml apps/macos/Sources/TcrBar/FleetView.swift 2>/dev/null
check "version bumped past the tag -> ALLOW" no
git reset -q && git checkout HEAD -- Cargo.toml

# 3. NEGATIVE CONTROL — docs-only commit at a released version is exempt.
git reset -q
printf '\n' >>README.md
git add README.md
check "docs-only at released version -> ALLOW" no
git reset -q && git checkout HEAD -- README.md

# 4. NEGATIVE CONTROL — nothing staged at all.
git reset -q
check "empty index -> ALLOW" no

echo
echo "  $pass passed, $fail failed"
[ "$fail" -eq 0 ]
