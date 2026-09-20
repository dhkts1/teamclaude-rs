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

pass() {
  ASSERT_TOTAL=$((ASSERT_TOTAL + 1))
  echo "$SCENARIO: PASS: $*"
}

fail() {
  ASSERT_TOTAL=$((ASSERT_TOTAL + 1))
  ASSERT_FAILURES=$((ASSERT_FAILURES + 1))
  echo "$SCENARIO: FAIL: $*"
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

# expect_log <container> <regex> <what>
# Passes when the container's boot log carries a line matching the regex.
# Prints the matched line as part of the PASS, because a scenario's evidence is
# the line, not the exit code of grep.
expect_log() {
  container="$1"
  pattern="$2"
  what="$3"
  line="$(docker exec "$container" grep -hE "$pattern" /scratch/boot.log 2>/dev/null | tail -1)"
  if [ -n "$line" ]; then
    pass "$what: $line"
  else
    fail "$what: no line matching /$pattern/ in $container:/scratch/boot.log"
  fi
}

# refute_log <container> <regex> <what>
# The inverse, for the facts that are about an absence. An absence is only
# evidence when the log has something in it at all, so this fails a log that is
# empty rather than reporting a clean absence from a file that never got written.
refute_log() {
  container="$1"
  pattern="$2"
  what="$3"
  if ! docker exec "$container" test -s /scratch/boot.log 2>/dev/null; then
    fail "$what: $container:/scratch/boot.log is empty, so its absence proves nothing"
    return
  fi
  line="$(docker exec "$container" grep -hE "$pattern" /scratch/boot.log 2>/dev/null | tail -1)"
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
