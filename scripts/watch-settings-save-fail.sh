#!/usr/bin/env bash
# Break `save_settings` four ways and confirm each break reddens the test that
# exists to catch it.
#
# Break 4 is why this script exists, and it caught a bad test before it shipped.
# Pointing `save_after_edit` back at `config::save` leaves EVERY config-level
# test green, because those call `save_settings` directly. The first cli-level
# test drove the whole `edit_account` chain and ALSO stayed green, because
# `edit_account` runs `load_for_edit` itself and so reads the file after any
# rotation. The test now calls `save_after_edit` directly, which is where the
# window actually is.
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
#   RUSTC_WRAPPER= CARGO_BUILD_RUSTC_WRAPPER= scripts/watch-settings-save-fail.sh
set -uo pipefail

cd "$(git rev-parse --show-toplevel)" || exit 1

CONFIG="src/config.rs"
CLI="src/cli.rs"
BACKUP_DIR="$(mktemp -d -t settings-save.XXXXXX)"
cp "$CONFIG" "$BACKUP_DIR/config.rs"
cp "$CLI" "$BACKUP_DIR/cli.rs"

restore() {
  cp "$BACKUP_DIR/config.rs" "$CONFIG"
  cp "$BACKUP_DIR/cli.rs" "$CLI"
  # mtime must move or cargo reuses the sabotaged build; `cp` gives a fresh one.
  touch "$CONFIG" "$CLI"
  rm -rf "$BACKUP_DIR"
}
trap restore EXIT

fails=0

# run_break <label> <file> <must-fail-test> <old::=new>
run_break() {
  local label="$1" file="$2" must_fail="$3" prog="$4" log
  log="$(mktemp -t settings-break.XXXXXX)"
  cp "$BACKUP_DIR/config.rs" "$CONFIG"
  cp "$BACKUP_DIR/cli.rs" "$CLI"

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
  touch "$CONFIG" "$CLI"

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
cp "$BACKUP_DIR/config.rs" "$CONFIG"; cp "$BACKUP_DIR/cli.rs" "$CLI"; touch "$CONFIG" "$CLI"
control_log="$(mktemp -t settings-control.XXXXXX)"
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

# 1. Credentials stop being carried from disk at all.
run_break "no-carry" "$CONFIG" \
  "a_settings_edit_does_not_revert_a_rotated_credential" \
  'carry_disk_credentials(&mut doc, &disk, &config.accounts);::='

# 2. A key drops out of the protected list. The drift guard must see it even
#    though `accessToken` alone would still let the refresh-token test pass.
run_break "shrink-key-list" "$CONFIG" \
  "the_credential_key_list_matches_what_merge_tokens_writes" \
  'const CREDENTIAL_KEYS: [&str; 3] = ["accessToken", "refreshToken", "expiresAt"];::=const CREDENTIAL_KEYS: [&str; 2] = ["accessToken", "refreshToken"];'

# 3. An absent-on-disk credential is left as the snapshot had it, reinstating
#    something the file no longer carries.
run_break "keep-on-absent" "$CONFIG" \
  "a_credential_absent_on_disk_is_removed_rather_than_reinstated" \
  'None => {
                    into.remove(key);
                }::=None => {}'

# 4. THE wiring break: the fix exists but the CLI does not use it.
run_break "save-after-edit-unwired" "$CLI" \
  "save_after_edit_does_not_revert_a_rotated_credential" \
  'config::save_settings(config_path, config)::=config::save(config_path, config)'

echo
if [ "$fails" -eq 0 ]; then
  echo "ALL BREAKS CAUGHT."
else
  echo "${fails} BREAK(S) NOT CAUGHT, see above."
fi
exit "$fails"
