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

  if [ -n "$hits" ]; then
    printf '%s\n' "$hits"
    return 1
  fi
  return 0
}

# tcr_disclosure_staged_added
# The ADDED lines of the staged changes, so pre-existing content can never block
# you. The denylist file itself is excluded: it holds the names on purpose.
tcr_disclosure_staged_added() {
  git diff --cached -U0 --diff-filter=ACM -- . ':(exclude).githooks/private-names' \
    | sed -n 's/^+//p' || true
}

# tcr_disclosure_commit_added <sha>
# The ADDED lines a single commit introduces. `git show` handles a root commit,
# which a `<sha>^..<sha>` range does not.
tcr_disclosure_commit_added() {
  local sha="${1:-}"
  [ -n "$sha" ] || return 0
  git show "$sha" --format= -U0 --diff-filter=ACM -- . ':(exclude).githooks/private-names' \
    | sed -n 's/^+//p' || true
}
