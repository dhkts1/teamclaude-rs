#!/usr/bin/env bash
# disclosure-scan — the public-repo disclosure checks, in one place.
#
# Three callers share it:
#   .githooks/pre-commit        staged changes, before a commit is written
#   .githooks/pre-merge-commit  the same, for a merge (git runs THIS, not pre-commit)
#   .githooks/pre-push          the outgoing commits: their diffs AND their messages
#
# WHY IT IS A LIBRARY. The checks lived inline in pre-commit. A commit is not the
# only way content reaches a public remote: a merge commit's content is never
# staged, so pre-commit never sees it, and a commit MESSAGE is not part of any
# diff, so no version of the staged scan could read one. Answering "would this
# disclose something?" in three spellings is how the three answers drift, so the
# checks live here and the hooks decide only WHAT text to hand over.
#
# Every grep is `|| true`: under `set -e` a no-match (exit 1) would abort the
# hook, and `grep -q` in a pipe can invert on SIGPIPE. Both failure modes report
# the wrong answer confidently, which is the defect this gate exists to prevent,
# one level up.
#
# shellcheck shell=bash

# The internal-scaffolding token list (check 4, below), lifted out to one
# place so `.githooks/pre-commit`'s staged-diff check and
# `scripts/check-public-disclosure.sh`'s tracked-tree / PR-title-and-body
# check (the CI-side gate, for forks that never run our hooks) scan for
# EXACTLY the same tokens rather than two lists that can drift apart.
# `\b` is a GNU extension and is NOT in POSIX ERE. Apple's git (2.50.1,
# Apple Git-155) silently matches nothing for it, so on macOS the three
# word-boundary tokens below were inert: `git grep -E '\bswarm\b'` finds zero
# hits in a .gitignore that GNU grep matches twice. Every macOS developer got a
# clean local scan while CI, on glibc, saw the hits. That is how #267 shipped a
# tree scan that turned main red on its first run: its author could not have
# seen it locally.
#
# `(^|[^[:alnum:]_])tok([^[:alnum:]_]|$)` is the portable spelling. `-P` also
# works here but depends on git being built with PCRE, which is not guaranteed
# on a contributor's machine.
_w_pre='(^|[^[:alnum:]_])'
_w_post='([^[:alnum:]_]|$)'
TCR_SCAFFOLDING_PATTERN="data/plans/|-bridge\\.md|${_w_pre}coordinator${_w_post}|${_w_pre}lane [abc]${_w_post}|${_w_pre}swarm${_w_post}"
unset _w_pre _w_post

# The list is local-only and gitignored, so no clone or worktree carries it: a
# committed list of the real names would itself be the disclosure this gate
# exists to stop. A MISSING list is a hard failure for the same reason a missing
# gitleaks is — its absence means the checkout is damaged or somebody deleted it,
# not that there is nothing to check. Skipping silently would leave the other two
# checks running and the whole gate looking healthy while the one check that
# knows the actual private names had been switched off.
tcr_disclosure_require_names_list() {
  local gate="${1:-gate}"
  if [ ! -f .githooks/private-names ] && [ "${TCR_ALLOW_MISSING_PRIVATE_NAMES:-0}" != "1" ]; then
    {
      echo ""
      echo "$gate: BLOCKED — .githooks/private-names is missing, so the private-name"
      echo "  half of the disclosure scan cannot run."
      echo ""
      echo "  The file is local-only and gitignored, so no clone or worktree carries it: a"
      echo "  committed list of the real names would itself be the disclosure this gate"
      echo "  exists to stop. Each checkout writes its own."
      echo ""
      echo "  Maintaining a list — one name or string per line, '#' comments allowed:"
      echo ""
      echo "      \$EDITOR .githooks/private-names"
      echo ""
      echo "  Contributing from a fork, with no private names of your own to protect? Then"
      echo "  this half is not yours to run, and skipping it is the correct answer:"
      echo ""
      echo "      TCR_ALLOW_MISSING_PRIVATE_NAMES=1 git commit ..."
      echo ""
    } >&2
    return 1
  fi
  return 0
}

# tcr_disclosure_scan_text <what>
# Reads text on stdin. Prints a human-readable hit report and returns 1 when the
# text would disclose something; prints nothing and returns 0 when it is clean.
# <what> names the text in the report ("staged changes", "commit message …") so
# a caller never has to compose the wording and the three callers cannot word the
# same finding three ways.
tcr_disclosure_scan_text() {
  local what="${1:-text}"
  local text hits="" found
  text="$(cat || true)"
  # Nothing to scan is clean. Said once, here, so no caller has to guess whether
  # an empty diff means "safe" or "the scan did not run".
  [ -n "$text" ] || return 0

  # 1. Private names — an editable list, so adding one needs no hook edit.
  if [ -f .githooks/private-names ]; then
    local name
    while IFS= read -r name || [ -n "$name" ]; do
      case "$name" in ''|\#*) continue ;; esac
      found="$(printf '%s\n' "$text" | grep -i -m3 -- "$name" || true)"
      [ -n "$found" ] && hits="$hits
  private name '$name' in $what:
$(printf '%s\n' "$found" | sed 's/^/      /')"
    done < .githooks/private-names
  fi

  # 2. Absolute home paths — they leak the machine layout and the operator's name.
  #    Obviously-synthetic users are allowed so fixtures and docs still work.
  found="$(printf '%s\n' "$text" \
           | grep -E -m3 '/(Users|home)/[A-Za-z0-9._-]+/' \
           | grep -v -i -E '/(Users|home)/(x|y|alice|bob|carol|test|example|runner|user)/' || true)"
  [ -n "$found" ] && hits="$hits
  absolute home path in $what:
$(printf '%s\n' "$found" | sed 's/^/      /')"

  # 3. Real-looking email addresses (example.com and the GitHub noreply are fine).
  #    Matched case-insensitively: example.com is RFC 2606 reserved, so
  #    ALICE@EXAMPLE.COM is no more real than the lowercase form.
  found="$(printf '%s\n' "$text" \
           | grep -E -o -m3 '[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}' \
           | grep -v -i -E '@(example\.(com|org|net)|users\.noreply\.github\.com)$' || true)"
  [ -n "$found" ] && hits="$hits
  real-looking email address in $what:
$(printf '%s\n' "$found" | sed 's/^/      /')"

  # 4. Internal (Henry) scaffolding — a citation of an untracked, machine-local
  #    path (`data/plans/…`, any `*-bridge.md`) is a dangling pointer for a
  #    public reader, and "coordinator"/"Lane A/B/C"/"swarm" name our internal
  #    multi-agent process rather than a fact about this codebase. `.githooks/**`
  #    is excluded from the text this function ever sees (see the two functions
  #    below) precisely so this gate's own source can describe, in prose, the
  #    tokens it blocks without tripping on itself.
  found="$(printf '%s\n' "$text" \
           | grep -E -m3 -i "$TCR_SCAFFOLDING_PATTERN" || true)"
  [ -n "$found" ] && hits="$hits
  internal-scaffolding reference in $what:
$(printf '%s\n' "$found" | sed 's/^/      /')"

  # 5. Operational figures — an operator's own cost and volume, which is the
  #    half that anonymising NAMES does not protect.
  #
  #    2026-09-17: a render fixture was filled in by reading the live proxy.
  #    Emails, UUIDs and home paths were all faked, and checks 1 to 4 passed on
  #    every commit, because none of them looks at a number. What went public
  #    was per-session cost ($308.38, $126.66), request counts (1,654 and 837),
  #    token volumes (328,915,843 cache-read) and error counts. Repairing it
  #    took a history rewrite of `main`.
  #
  #    The rule keys on ROUNDNESS, not on magnitude. A fixture needs a figure of
  #    the right SIZE and never a real one, so an invented value is round by
  #    construction (1_500, 300.00, 300_000_000) and a measured one is not
  #    (1_654, 308.38, 328_915_843). More than two significant digits on a money
  #    or volume field is therefore a value somebody read off a running system.
  #
  #    It is a tripwire and not a proof: 4_100 and 96 were also real and both
  #    have two significant digits, so they pass. That is acceptable, because
  #    any single hit blocks the commit and puts a human in front of the whole
  #    fixture, which is all this needed to have done.
  #
  #    Escape hatch: `disclosure-ok:` and a reason, either at the end of the
  #    line or on the line directly above it. The line-above form exists because
  #    swift-format refuses an end-of-line comment that overruns the line length,
  #    so on an already-long fixture line a trailing marker cannot be written at
  #    all — found while sweeping the older fixtures, 2026-09-18.
  found="$(printf '%s\n' "$text" \
           | awk '
      /disclosure-ok:/ { exempt_next = 1; next }
      { if (exempt_next) { exempt_next = 0; next } }
      { print }' \
           | awk '
      match($0, /(costUsd|spendUsd|totalUsd|requests|calls|errors|timeouts|inputTokens|outputTokens|cacheReadTokens|cacheCreationTokens)[[:space:]]*[:=][[:space:]]*[0-9][0-9_.]*/) {
        frag = substr($0, RSTART, RLENGTH)
        val = frag
        sub(/^[A-Za-z]+[[:space:]]*[:=][[:space:]]*/, "", val)
        gsub(/[_.]/, "", val)
        sub(/^0+/, "", val)
        sub(/0+$/, "", val)
        if (length(val) > 2) { print; hits++ }
        if (hits >= 3) exit
      }' || true)"
  [ -n "$found" ] && hits="$hits
  operational figure (cost or volume read off a running system) in $what:
$(printf '%s\n' "$found" | sed 's/^/      /')"

  if [ -n "$hits" ]; then
    printf '%s\n' "$hits"
    return 1
  fi
  return 0
}

# tcr_disclosure_staged_added
# The ADDED lines of the staged changes, so pre-existing content can never block
# you. The denylist file itself is excluded: it holds the names on purpose.
# `.githooks/**` is excluded too: this library and its callers must describe,
# in their own comments, the exact tokens check 4 above blocks.
tcr_disclosure_staged_added() {
  git diff --cached -U0 --diff-filter=ACM -- . \
    ':(exclude).githooks/private-names' ':(exclude).githooks/**' \
    | sed -n 's/^+//p' || true
}

# tcr_disclosure_commit_added <sha>
# The ADDED lines a single commit introduces. `git show` handles a root commit,
# which a `<sha>^..<sha>` range does not.
tcr_disclosure_commit_added() {
  local sha="${1:-}"
  [ -n "$sha" ] || return 0
  git show "$sha" --format= -U0 --diff-filter=ACM -- . \
    ':(exclude).githooks/private-names' ':(exclude).githooks/**' \
    | sed -n 's/^+//p' || true
}
