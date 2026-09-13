#!/usr/bin/env bash
# Prove the two deflaked log assertions still catch a REAL missing line.
#
# This is the script that decides whether the previous commit was a deflake or a
# mute. Both tests were made tolerant of WHEN a line arrives: the boot-line test
# polls its sink for up to 5s instead of reading once, and the SIGTERM test
# accepts the line from the child's durable log as well as from the stdout pipe.
# Tolerance of timing must not become tolerance of absence. Each break below
# deletes the line at its source, so neither source can have it, and each test
# must still go red.
#
# `mktemp -t NAME.XXXXXX`, with the X's: GNU mktemp rejects a -t template
# without them; BSD mktemp accepts the bare name, so the bare form looks correct
# and is not.
#
# Plain `cargo`. If your machine wraps rustc with sccache and that server has
# wedged, every build hangs with no output and no rustc process; clear the
# wrapper for the run rather than teaching this script about one machine's cache:
#   RUSTC_WRAPPER= CARGO_BUILD_RUSTC_WRAPPER= scripts/watch-deflaked-log-assertions-fail.sh
set -uo pipefail

cd "$(git rev-parse --show-toplevel)" || exit 1

SERVER="src/server.rs"
MAIN="src/main.rs"
BACKUP_DIR="$(mktemp -d -t deflake.XXXXXX)"
cp "$SERVER" "$BACKUP_DIR/server.rs"
cp "$MAIN" "$BACKUP_DIR/main.rs"

restore() {
  cp "$BACKUP_DIR/server.rs" "$SERVER"
  cp "$BACKUP_DIR/main.rs" "$MAIN"
  touch "$SERVER" "$MAIN"
  rm -rf "$BACKUP_DIR"
}
trap restore EXIT

fails=0

# run_break <label> <file> <cargo-target-args> <must-fail-test> <old::=new>
run_break() {
  local label="$1" file="$2" target="$3" must_fail="$4" prog="$5" log
  log="$(mktemp -t deflake-break.XXXXXX)"
  cp "$BACKUP_DIR/server.rs" "$SERVER"
  cp "$BACKUP_DIR/main.rs" "$MAIN"

  if ! python3 - "$file" "$prog" <<'PY'
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
  touch "$SERVER" "$MAIN"

  # shellcheck disable=SC2086
  cargo test $target "$must_fail" >"$log" 2>&1
  if grep -qE "^test .*${must_fail} \.\.\. FAILED" "$log"; then
    echo "BREAK-${label}: OK, ${must_fail} went red."
  else
    echo "BREAK-${label}: MASKED, ${must_fail} did NOT fail. The deflake swallowed a real miss."
    grep -E '^test |^error|test result' "$log" | sed 's/^/    /' | head -6
    fails=$((fails + 1))
  fi
  rm -f "$log"
}

echo "=== control: both guards must RUN and pass on the unbroken tree ==="
cp "$BACKUP_DIR/server.rs" "$SERVER"; cp "$BACKUP_DIR/main.rs" "$MAIN"; touch "$SERVER" "$MAIN"
c1="$(mktemp -t deflake-c1.XXXXXX)"; c2="$(mktemp -t deflake-c2.XXXXXX)"
cargo test --lib boot_line_carries_every_new_knob >"$c1" 2>&1
cargo test --test headless_sigterm >"$c2" 2>&1
ok=1
grep -qE '^test .*boot_line_carries_every_new_knob.* \.\.\. ok' "$c1" || { echo "CONTROL: boot-line guard did not run or pass."; ok=0; }
grep -qE '^test .*a_supervised_sigterm.* \.\.\. ok' "$c2" || { echo "CONTROL: sigterm guard did not run or pass."; ok=0; }
rm -f "$c1" "$c2"
if [ "$ok" -eq 0 ]; then
  echo "CONTROL: BLOCKED, nothing below means anything."
  exit 1
fi
echo "CONTROL: OK, both guards ran and passed."

# 1. The boot line is never emitted. A 5s poll must still fail, not hang and pass.
run_break "no-boot-line" "$SERVER" "--lib" \
  "boot_line_carries_every_new_knob_with_its_configured_value" \
  '        "server started"::=        "server has started"'

# 2. The SIGTERM line is never emitted, so NEITHER stdout nor the durable log
#    can carry it. Accepting a second source must not mean accepting none.
run_break "no-sigterm-line" "$MAIN" "--test headless_sigterm" \
  "a_supervised_sigterm_drains_before_the_process_exits" \
  'Self::Sigterm => "SIGTERM received; shutting down",::=Self::Sigterm => "stopping",'

echo
if [ "$fails" -eq 0 ]; then
  echo "ALL BREAKS CAUGHT. The deflakes tolerate timing, not absence."
else
  echo "${fails} BREAK(S) MASKED, see above."
fi
exit "$fails"
