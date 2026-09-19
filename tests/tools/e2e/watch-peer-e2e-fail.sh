#!/usr/bin/env bash
# watch-peer-e2e-fail.sh: prove tests/peer_e2e.rs is a gate and not a passenger.
#
# A test that has only ever passed proves nothing about what it guards. This
# breaks the two production lines the end-to-end depends on, one at a time,
# and refuses to exit 0 unless each break turns the matching test RED with the
# message that names it:
#
#   1. src/server.rs           the peer-lease fallback install at boot
#      -> a_borrowed_request_... fails: the borrower's boot installs no provider
#   2. src/peer/listener.rs    the six digits the RESPONDER shows its operator
#      -> two_macs_..._pair_on_six_digits fails: no digits to compare against
#
# The file being mutated is copied BYTE FOR BYTE to a backup first and restored
# from that copy, never with `git checkout` / `git restore`: a restore that
# reaches for git wipes every other uncommitted edit in the tree, which is a
# cure worse than the mutation. The restore runs from an EXIT trap, so a Ctrl-C
# or a failed build still puts the file back, and the digest of the restored
# file is compared with the backup's before this script reports anything. A
# missing digest tool is a hard failure here, never a comparison that passes
# vacuously because both sides came back empty.
#
# Usage:  tests/tools/e2e/watch-peer-e2e-fail.sh [worktree-root]
# Needs:  a `cargo` that can build this tree, and `md5` (macOS) or `md5sum`
#         (everywhere else). Honours CARGO_TARGET_DIR.
set -uo pipefail

# digest_of <path>: `md5` on macOS, `md5sum` elsewhere; a missing tool exits
# hard rather than letting a restore-compare read two empty strings as equal.
if command -v md5 >/dev/null 2>&1; then
  digest_of() { md5 -q "$1"; }
elif command -v md5sum >/dev/null 2>&1; then
  digest_of() { md5sum < "$1" | cut -d' ' -f1; }
else
  echo "FAIL: neither md5 nor md5sum is on PATH; cannot verify the restore" >&2
  exit 8
fi

ROOT="${1:-$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)}"
MANIFEST="$ROOT/Cargo.toml"
[ -f "$MANIFEST" ] || { echo "FAIL: no Cargo.toml at $MANIFEST" >&2; exit 2; }
LOGDIR="${TMPDIR:-/tmp}/peer-e2e-mutation-$$"
mkdir -p "$LOGDIR"
echo "logs: $LOGDIR"

BACKUP=""
TARGET=""
restore() {
  if [ -n "$BACKUP" ] && [ -n "$TARGET" ] && [ -f "$BACKUP" ]; then
    cp "$BACKUP" "$TARGET"
    before="$(digest_of "$BACKUP")"
    after="$(digest_of "$TARGET")"
    if [ -z "$before" ] || [ -z "$after" ] || [ "$before" != "$after" ]; then
      echo "FAIL: $TARGET was NOT restored to its original bytes (before=${before:-<empty>} after=${after:-<empty>}); the copy is at $BACKUP" >&2
      exit 3
    fi
    echo "restored $TARGET"
  fi
  BACKUP=""; TARGET=""
}
trap restore EXIT

# mutate <file> <python-expression-file>: applies a literal string swap.
mutate() { # $1 file  $2 needle  $3 replacement
  TARGET="$1"
  BACKUP="$LOGDIR/$(basename "$1").orig"
  cp "$TARGET" "$BACKUP"
  NEEDLE="$2" REPLACEMENT="$3" TARGET_FILE="$TARGET" python3 - <<'PY'
import os, sys
path = os.environ["TARGET_FILE"]
needle = os.environ["NEEDLE"]
replacement = os.environ["REPLACEMENT"]
body = open(path).read()
hits = body.count(needle)
if hits != 1:
    sys.exit(f"the mutation needle matched {hits} times in {path}, not once; refusing to guess")
open(path, "w").write(body.replace(needle, replacement))
PY
  local status=$?
  [ "$status" -eq 0 ] || { echo "FAIL: could not apply the mutation to $1" >&2; exit 4; }
  echo "mutated $1"
}

# expect_red <test-name> <expected-text-in-the-failure> <log-name>
expect_red() {
  local test_name="$1" expected="$2" log="$LOGDIR/$3"
  cargo test --manifest-path "$MANIFEST" --test peer_e2e "$test_name" \
    -- --nocapture --test-threads=1 > "$log" 2>&1
  local status=$?
  if [ "$status" -eq 0 ]; then
    echo "FAIL: $test_name still PASSED with the production line broken: it is not a gate." >&2
    echo "      log: $log" >&2
    exit 5
  fi
  if ! grep -qF "$expected" "$log"; then
    echo "FAIL: $test_name failed, but not on the expected assertion." >&2
    echo "      wanted: $expected" >&2
    echo "      log: $log" >&2
    exit 6
  fi
  echo "RED as expected: $test_name ($expected)"
}

expect_green() {
  local log="$LOGDIR/restored.log"
  cargo test --manifest-path "$MANIFEST" --test peer_e2e -- --test-threads=1 > "$log" 2>&1
  local status=$?
  [ "$status" -eq 0 ] || { echo "FAIL: the restored tree is not green; log: $log" >&2; exit 7; }
  echo "GREEN with both lines restored"
}

# --- mutation 1: the borrower's boot installs no peer-lease provider ---------
mutate "$ROOT/src/server.rs" \
  'match crate::fallback::install_peer_lease_provider(&peers_path) {' \
  'match Ok::<_, anyhow::Error>(crate::fallback::Installed::NothingToBorrowFrom) {'
expect_red a_borrowed_request_reaches_the_lender_and_never_carries_the_borrowers_credential \
  "the borrower's boot must install a provider now that it has a lender" mutation-1.log
restore

# --- mutation 2: the responder never shows its operator the six digits ------
mutate "$ROOT/src/peer/listener.rs" \
  '            code = %session.session.code,
' \
  ''
expect_red two_macs_find_each_other_pair_on_six_digits_and_pin \
  "no six digits in" mutation-2.log
restore

expect_green
echo "PASS: tests/peer_e2e.rs catches both breaks and passes when they are undone"
