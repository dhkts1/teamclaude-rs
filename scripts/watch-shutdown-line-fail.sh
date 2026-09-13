#!/usr/bin/env bash
# Break the shutdown-line contract two ways and confirm the guard reddens.
#
# `--bin tcr`, not `--lib`. These tests live in `src/main.rs`, which is the
# BINARY target; `cargo test --lib every_shutdown_trigger_logs_a_line` exits 0
# having run NOTHING and filtered all 1095 lib tests out. An exit code alone
# cannot tell that apart from a pass, which is why this script greps for the
# test name in the output rather than trusting the status.
#
# `mktemp -t NAME.XXXXXX`, with the X's: GNU mktemp rejects a -t template
# without them; BSD mktemp accepts the bare name, so the bare form looks correct
# and is not.
#
# Plain `cargo`. If your machine wraps rustc with sccache and that server has
# wedged, every build hangs with no output and no rustc process; clear the
# wrapper for the run rather than teaching this script about one machine's cache:
#   RUSTC_WRAPPER= CARGO_BUILD_RUSTC_WRAPPER= scripts/watch-shutdown-line-fail.sh
set -uo pipefail

cd "$(git rev-parse --show-toplevel)" || exit 1

MAIN="src/main.rs"
BACKUP="$(mktemp -t main-rs.XXXXXX)"
cp "$MAIN" "$BACKUP"

restore() {
  cp "$BACKUP" "$MAIN"
  touch "$MAIN"
  rm -f "$BACKUP"
}
trap restore EXIT

TEST="every_shutdown_trigger_logs_a_line"
fails=0

run_break() {
  local label="$1" prog="$2" log
  log="$(mktemp -t shutdown-break.XXXXXX)"
  cp "$BACKUP" "$MAIN"
  if ! python3 - "$MAIN" "$prog" <<'PY'
import pathlib, sys
p = pathlib.Path(sys.argv[1])
old, new = sys.argv[2].split("::=")
s = p.read_text()
if old not in s:
    sys.exit(f"BREAK NOT APPLIED: {old!r} not found in {p}")
p.write_text(s.replace(old, new, 1))
PY
  then
    echo "BREAK-${label}: BLOCKED, the sabotage did not apply."
    fails=$((fails + 1)); rm -f "$log"; return
  fi
  touch "$MAIN"

  cargo test --bin tcr "$TEST" >"$log" 2>&1
  if grep -qE "^test .*${TEST} \.\.\. FAILED" "$log"; then
    echo "BREAK-${label}: OK, ${TEST} went red."
  else
    echo "BREAK-${label}: HOLE, ${TEST} did NOT fail."
    grep -E '^test |^error|test result' "$log" | sed 's/^/    /' | head -6
    fails=$((fails + 1))
  fi
  rm -f "$log"
}

echo "=== control: the guard must RUN and pass on the unbroken tree ==="
cp "$BACKUP" "$MAIN"; touch "$MAIN"
control_log="$(mktemp -t shutdown-control.XXXXXX)"
cargo test --bin tcr "$TEST" >"$control_log" 2>&1
if grep -qE "^test .*${TEST} \.\.\. ok" "$control_log"; then
  echo "CONTROL: OK, the guard ran and passed."
  rm -f "$control_log"
else
  echo "CONTROL: BLOCKED, the guard did not run or did not pass; nothing below means anything."
  grep -E '^test |test result|^error' "$control_log" | sed 's/^/    /' | head -8
  rm -f "$control_log"
  exit 1
fi

# 1. The silent branch comes back.
run_break "servingstopped-silent" \
  'Self::ServingStopped => "serving stopped on its own; shutting down",::=Self::ServingStopped => "",'

# 2. Two triggers share a line, which would let the SIGTERM assertion in
#    tests/headless_sigterm.rs pass for the wrong trigger.
run_break "duplicate-line" \
  'Self::ServingStopped => "serving stopped on its own; shutting down",::=Self::ServingStopped => "SIGTERM received; shutting down",'

echo
if [ "$fails" -eq 0 ]; then
  echo "ALL BREAKS CAUGHT."
else
  echo "${fails} BREAK(S) NOT CAUGHT, see above."
fi
exit "$fails"
