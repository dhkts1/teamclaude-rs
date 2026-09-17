#!/usr/bin/env bash
# Decide whether a set of changed paths can possibly affect a build or a test.
#
# WHY. A nine-line edit to apps/macos/appcast.xml spent 715 seconds of CI on
# 2026-09-17 (#341): macos 294s, ci 179s, tsan 122s, miri 88s, none of which can
# read an XML file the build does not compile. This script is the discriminator
# that lets ci.yml skip those four.
#
# The rule is deliberately ONE-SIDED. A path is inert only if it appears on the
# list below; anything unrecognised is code. A new directory nobody thought about
# therefore runs the full suite, which is the safe direction to be wrong in: the
# cost of a false "code" is four minutes, and the cost of a false "inert" is a
# merged regression nobody tested.
#
# Reads paths on stdin, one per line. Prints `inert` or `code`.
# `--self-check` runs the fixture below and exits non-zero if the rule moved.
set -euo pipefail

is_inert_path() {
  case "$1" in
    *.md)                     return 0 ;;
    docs/*)                   return 0 ;;
    apps/macos/appcast.xml)   return 0 ;;
    assets/*)                 return 0 ;;
    LICENSE|.gitignore|.gitattributes|CODEOWNERS) return 0 ;;
    *)                        return 1 ;;
  esac
}

classify() {
  local any=0 path
  while IFS= read -r path; do
    [ -n "$path" ] || continue
    any=1
    if ! is_inert_path "$path"; then
      echo code
      return 0
    fi
  done
  # No paths at all means nothing to judge; run everything rather than guess.
  [ "$any" = 1 ] && echo inert || echo code
}

self_check() {
  local fails=0
  check() { # expected, label, paths...
    local want="$1" label="$2"; shift 2
    local got; got="$(printf '%s\n' "$@" | classify)"
    if [ "$got" != "$want" ]; then
      echo "  FAIL $label: want $want, got $got" >&2; fails=$((fails+1))
    else
      echo "  ok   $label -> $got"
    fi
  }
  # The case this exists for.
  check inert "the appcast entry (#341)"        apps/macos/appcast.xml
  check inert "a README edit"                   README.md
  check inert "docs and a markdown file"        docs/RELEASING.md CONTRIBUTING.md
  check inert "a screenshot"                    assets/tcrbar-panel-fleet.png
  # Must NOT be inert. A gate that cannot say `code` is not a gate.
  check code  "rust source"                     src/session_wire.rs
  check code  "swift source"                    apps/macos/Sources/TcrBar/FleetView.swift
  check code  "a workflow"                      .github/workflows/ci.yml
  check code  "Cargo.lock"                      Cargo.lock
  check code  "a hook"                          .githooks/pre-commit
  check code  "a script"                        scripts/install-cli.sh
  check code  "an unknown new directory"        newthing/whatever.txt
  # Mixed: one code path poisons the whole set.
  check code  "docs PLUS one rust file"         docs/cli.md src/main.rs
  check code  "appcast PLUS a swift file"       apps/macos/appcast.xml apps/macos/Sources/TcrBarCore/FleetStatus.swift
  # Degenerate.
  check code  "no paths at all"                 ""
  if [ "$fails" -ne 0 ]; then
    echo "changed-is-inert: $fails fixture case(s) failed" >&2; exit 1
  fi
  echo "changed-is-inert: self-check ok (14 cases)"
}

case "${1:-}" in
  --self-check) self_check ;;
  -h|--help)    sed -n '2,18p' "$0" | sed 's/^# \{0,1\}//' ;;
  *)            classify ;;
esac
