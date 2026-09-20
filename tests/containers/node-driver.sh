#!/bin/sh
# The one CLI dance a scenario cannot drive from outside the container:
# `tcr peer pair`, which knocks, waits minutes for somebody at the other Mac to
# press Accept, and then reads six digits off standard input.
#
# Why a file in the image rather than a string of `docker exec` calls. Each
# `docker exec` is a process of its own, so the standard input of one is gone
# by the time the next runs. `tcr peer pair` reads its line LAST, after an
# Accept that has not happened yet when the command starts, so something inside
# the container has to hold the write end of that stream open across the whole
# wait. A fifo plus a writer that blocks until the digits are known is that
# something, and it is the same shape `tests/peer_e2e.rs` gets for free by
# keeping the child's `Stdio::piped()` stdin alive in the test process.
#
# Verbs, each one `docker exec`ed on its own:
#   pair-start <addr>   knock at <addr> and start waiting (run it detached)
#   pair-instance       the boot instance id the knock announced, for `accept`
#   pair-code           the six digits this Mac is showing
#   pair-answer <code>  hand the compared digits to the waiting process
#   pair-wait           block until it finishes, print everything, exit its code
#
# `--json` throughout, so what is read back is the CLI's own event line rather
# than an English sentence this file would have to keep in step with.
set -eu

PEERS=/scratch/.config/tcr-peers.json
FIFO=/scratch/pair.in
OUT=/scratch/pair.out
CODE=/scratch/pair.code
STATUS=/scratch/pair.status
# Ten minutes is `pair::PAIR_WAIT`; these are the outside of the two waits a
# scenario does around it, and a scenario that hits one has found something.
WAIT_SECONDS=180

# Poll until the command in `$@` succeeds, up to WAIT_SECONDS. One loop rather
# than three, so every wait in this file has the same deadline and the same
# refusal. A command and not a string through `eval`: the conditions here are
# already shell functions, and a quoted expression would be re-parsed once more
# than it was written.
wait_until() {
  i=0
  while [ "$i" -lt "$WAIT_SECONDS" ]; do
    if "$@"; then
      return 0
    fi
    sleep 1
    i=$((i + 1))
  done
  return 1
}

# One `--json` event field off the pair process's output, newest last.
event_field() {
  # $1: the event name, $2: the field name.
  sed -n "s/.*\"event\":\"$1\"[^}]*\"$2\":\"\([^\"]*\)\".*/\1/p" "$OUT" 2>/dev/null | tail -1
}

have_field() { [ -n "$(event_field "$1" "$2")" ]; }
have_status() { [ -s "$STATUS" ]; }

case "${1:-}" in
  pair-start)
    addr="${2:?pair-start needs an address to knock at}"
    rm -f "$FIFO" "$OUT" "$CODE" "$STATUS"
    mkfifo "$FIFO"
    : > "$OUT"
    # Opening a fifo for writing blocks until a reader opens it, and for
    # reading until a writer does, so both ends start here and neither is
    # waited on. The writer parks until `pair-answer` has written the digits,
    # which is what keeps the pair process's stdin open through the wait.
    ( while [ ! -s "$CODE" ]; do sleep 1; done; cat "$CODE" ) > "$FIFO" 2>/dev/null </dev/null &
    # The status is recorded through an `if` and not after the command: a
    # subshell inherits `set -e`, so a refused pairing killed the subshell
    # before it could write its own exit code, and `pair-wait` then sat out its
    # whole deadline over a refusal that had already been printed. Watched: the
    # wrong-digits run said `driver: pair did not finish within 180s` under a
    # `refused` event that was right there in the log.
    ( if tcr peer pair "$addr" --peers "$PEERS" --json < "$FIFO" > "$OUT" 2>&1; then
        echo 0 > "$STATUS"
      else
        echo "$?" > "$STATUS"
      fi ) >/dev/null 2>&1 </dev/null &
    echo "driver: pair-start addr=$addr"
    ;;
  pair-instance)
    if wait_until have_field asking instance; then
      event_field asking instance
    else
      echo "driver: no asking event in $OUT after ${WAIT_SECONDS}s" >&2
      cat "$OUT" >&2
      exit 1
    fi
    ;;
  pair-code)
    if wait_until have_field comparing code; then
      event_field comparing code
    else
      echo "driver: no comparing event in $OUT after ${WAIT_SECONDS}s" >&2
      cat "$OUT" >&2
      exit 1
    fi
    ;;
  pair-answer)
    code="${2:?pair-answer needs the six digits}"
    # Written whole and then moved: the writer above wakes on a non-empty file,
    # and a half-written one would be answered as the digits.
    printf '%s\n' "$code" > "$CODE.part"
    mv "$CODE.part" "$CODE"
    echo "driver: pair-answer code=$code"
    ;;
  pair-wait)
    if wait_until have_status; then
      cat "$OUT"
      status="$(cat "$STATUS")"
      echo "driver: pair exit=$status"
      exit "$status"
    fi
    cat "$OUT"
    echo "driver: pair did not finish within ${WAIT_SECONDS}s" >&2
    exit 1
    ;;
  *)
    echo "driver: no verb named ${1:-<none>} (pair-start, pair-instance, pair-code, pair-answer, pair-wait)" >&2
    exit 2
    ;;
esac
