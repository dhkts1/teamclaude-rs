#!/usr/bin/env bash
# Break `CredentialScope` three ways and confirm each break reddens the test
# that exists to catch it.
#
# The third break is the one that justifies the script. `persist_tokens` handing
# over `CredentialScope::All` leaves EVERY config-level test green, because they
# call `save_tokens_for` directly. Only the manager-level test sees it. A fix
# that is available but not wired is the failure mode here.
#
# Both files are restored by a trap on EXIT, so an interrupt does not leave a
# sabotaged credential path in the tree.
#
# `mktemp -t NAME.XXXXXX`, with the X's: GNU mktemp rejects a -t template
# without them; BSD mktemp accepts the bare name, so the bare form looks correct
# and is not.
#
# Plain `cargo`. If your machine wraps rustc with sccache and that server has
# wedged, every build here hangs with no output and no rustc process; clear the
# wrapper for the run rather than teaching this script about one machine's cache:
#   RUSTC_WRAPPER= CARGO_BUILD_RUSTC_WRAPPER= scripts/watch-credential-scope-fail.sh
set -uo pipefail

cd "$(git rev-parse --show-toplevel)" || exit 1

CONFIG="src/config.rs"
MANAGER="src/manager/mod.rs"
BACKUP_DIR="$(mktemp -d -t cred-scope.XXXXXX)"
cp "$CONFIG" "$BACKUP_DIR/config.rs"
cp "$MANAGER" "$BACKUP_DIR/mod.rs"

restore() {
  cp "$BACKUP_DIR/config.rs" "$CONFIG"
  cp "$BACKUP_DIR/mod.rs" "$MANAGER"
  # mtime must move or cargo reuses the sabotaged build; `cp` gives a fresh one.
  touch "$CONFIG" "$MANAGER"
  rm -rf "$BACKUP_DIR"
}
trap restore EXIT

fails=0

# run_break <label> <file> <must-fail-test> <old::=new>
run_break() {
  local label="$1" file="$2" must_fail="$3" prog="$4" log
  log="$(mktemp -t cred-break.XXXXXX)"
  cp "$BACKUP_DIR/config.rs" "$CONFIG"
  cp "$BACKUP_DIR/mod.rs" "$MANAGER"

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
    echo "BREAK-${label}: BLOCKED, the sabotage did not apply; the result below would be meaningless."
    fails=$((fails + 1))
    rm -f "$log"
    return
  fi
  touch "$CONFIG" "$MANAGER"

  cargo test --lib "$must_fail" >"$log" 2>&1
  if grep -qE "^test .*${must_fail} \.\.\. FAILED" "$log"; then
    echo "BREAK-${label}: OK, ${must_fail} went red."
  else
    echo "BREAK-${label}: HOLE, ${must_fail} did NOT fail."
    grep -E '^test |^error' "$log" | sed 's/^/    /' | head -8
    fails=$((fails + 1))
  fi
  rm -f "$log"
}

echo "=== control: the unbroken tree must be green ==="
cp "$BACKUP_DIR/config.rs" "$CONFIG"; cp "$BACKUP_DIR/mod.rs" "$MANAGER"; touch "$CONFIG" "$MANAGER"
control_log="$(mktemp -t cred-control.XXXXXX)"
cargo test --lib >"$control_log" 2>&1
if grep -q '^test result: ok' "$control_log"; then
  echo "CONTROL: OK, the unbroken tree passes, so a red below is the break."
  rm -f "$control_log"
else
  echo "CONTROL: BLOCKED, the tree is already red; nothing below means anything."
  grep -E '^test .* FAILED|^error' "$control_log" | sed 's/^/    /' | head -10
  rm -f "$control_log"
  exit 1
fi

# 1. The scope stops narrowing anything.
run_break "covers-always-true" "$CONFIG" \
  "a_rotation_does_not_revert_another_processs_credential" \
  'Self::Only(only) => only == position,::=Self::Only(_) => true,'

# 2. The counter stops counting.
run_break "no-out-of-scope-count" "$CONFIG" \
  "a_scoped_write_counts_the_untouched_rather_than_calling_them_skipped" \
  'report.out_of_scope += 1;::='

# 3. THE wiring break: the fix exists but the rotation does not use it. Every
#    config-level test stays green; only the manager-level test sees this.
run_break "persist-tokens-unwired" "$MANAGER" \
  "a_rotation_persists_only_the_rotated_account" \
  'config::save_tokens_for(path, &snapshot, config::CredentialScope::Only(position))::=config::save_tokens_for(path, &snapshot, config::CredentialScope::All)'

echo
if [ "$fails" -eq 0 ]; then
  echo "ALL BREAKS CAUGHT."
else
  echo "${fails} BREAK(S) NOT CAUGHT, see above."
fi
exit "$fails"
