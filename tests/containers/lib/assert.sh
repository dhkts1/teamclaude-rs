#!/bin/sh
# The one assertion shape every scenario prints, and the counters behind it.
#
# One line per assertion, greppable and readable at a glance:
#
#     <scenario>: PASS: <the fact that held>
#     <scenario>: FAIL: <the fact that did not>
#
# A scenario sources this, calls `pass`/`fail` (or `expect`/`expect_log`), and
# ends with `finish`, which exits 0 only when nothing failed. Nothing here
# writes to a file: the runner captures stdout.
#
# SCENARIO is the name that leads every line. A scenario that forgets to set it
# gets a refusal rather than lines nobody can grep back to a file.
: "${SCENARIO:?a scenario must set SCENARIO before sourcing assert.sh}"

ASSERT_FAILURES=0
ASSERT_TOTAL=0

# One assertion is one line, whatever it was handed.
#
# The evidence an assertion carries is usually a command's whole output, and a
# refusal is the case where that output is several lines. Left alone, those
# lines land in the log without the scenario name on them, so the one thing
# every reader does with this harness, grep for FAIL, returns the first line of
# the failure and hides the rest of it. The newlines become separators and
# nothing is dropped.
one_line() { printf '%s' "$*" | tr '\n' '~' | tr -s '~' | sed 's/~/ | /g'; }

pass() {
  ASSERT_TOTAL=$((ASSERT_TOTAL + 1))
  printf '%s: PASS: %s\n' "$SCENARIO" "$(one_line "$@")"
}

fail() {
  ASSERT_TOTAL=$((ASSERT_TOTAL + 1))
  ASSERT_FAILURES=$((ASSERT_FAILURES + 1))
  printf '%s: FAIL: %s\n' "$SCENARIO" "$(one_line "$@")"
}

# expect <condition-description> <command...>
# Runs the command, passes when it exits 0.
expect() {
  what="$1"
  shift
  if "$@" >/dev/null 2>&1; then
    pass "$what"
  else
    fail "$what (command exited $?: $*)"
  fi
}

# expect_log <node> <regex> <what>
# Passes when the node's boot log carries a line matching the regex. Prints
# the matched line as part of the PASS, because a scenario's evidence is the
# line, not the exit code of grep. The boot log is read at /lab/$node/boot.log:
# past `docker exec`, which is the Docker dependency a reader of this file used
# to miss, and the parameter's own name now says what it holds.
expect_log() {
  node="$1"
  pattern="$2"
  what="$3"
  line="$(grep -hE "$pattern" "/lab/$node/boot.log" 2>/dev/null | tail -1)"
  if [ -n "$line" ]; then
    pass "$what: $line"
  else
    fail "$what: no line matching /$pattern/ in /lab/$node/boot.log"
  fi
}

# refute_log <node> <regex> <what>
# The inverse, for the facts that are about an absence. An absence is only
# evidence when the log has something in it at all, so this fails a log that is
# empty rather than reporting a clean absence from a file that never got written.
refute_log() {
  node="$1"
  pattern="$2"
  what="$3"
  if ! test -s "/lab/$node/boot.log"; then
    fail "$what: /lab/$node/boot.log is empty, so its absence proves nothing"
    return
  fi
  line="$(grep -hE "$pattern" "/lab/$node/boot.log" 2>/dev/null | tail -1)"
  if [ -n "$line" ]; then
    fail "$what: found $line"
  else
    pass "$what"
  fi
}

finish() {
  echo "$SCENARIO: $((ASSERT_TOTAL - ASSERT_FAILURES))/$ASSERT_TOTAL assertions held"
  [ "$ASSERT_FAILURES" -eq 0 ]
}

# The skeleton's stand-in. A scenario that has not been wired yet says so and
# exits 1: a harness whose unwritten scenarios exit 0 reads exactly like a
# harness that is passing.
not_wired() {
  fail "not wired yet: $*"
  finish
  exit 1
}
