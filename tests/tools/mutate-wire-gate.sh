#!/opt/homebrew/bin/bash
# Mutation-test the wire credential gates: no wire type may carry a
# credential field.
#
# Runs three probes against BOTH gates in tests/peer_wire.rs:
# the denylist (`wire_has_no_credential_field`) and the
# allowlist (`every_wire_type_serializes_only_allowlisted_keys`). Each probe
# adds a credential field to a real wire type: `MoveOffer`, "the only phase
# that moves a credential between hosts", which is exactly where a careless
# edit would put one: and fixes the one struct literal in the test file so the
# mutation COMPILES, because a mutation that only breaks the build proves
# nothing about the gate.
#
# The allowlist must go red on all three. The denylist is expected to pass two
# of them; that is the finding the allowlist exists for, and it is printed
# rather than hidden.
#
# Backups are byte copies restored with `cp`, never `git checkout`: this tree
# holds uncommitted work from the rest of the run.
set -uo pipefail

# The repository this script belongs to, derived from the script's own location:
# a hardcoded absolute path would be one reader's home directory in a PUBLIC repo,
# and wrong in every other checkout.
ROOT=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
LIB="$ROOT/crates/tcr-peer-wire/src/lib.rs"
TEST="$ROOT/tests/peer_wire.rs"
STAMP="$$-$(date +%s)"
BACKUP_DIR="/tmp/lan-p2p/mutate-wire-$STAMP"
LOG_DIR="/tmp/lan-p2p"
mkdir -p "$BACKUP_DIR" "$LOG_DIR"
cp "$LIB" "$BACKUP_DIR/lib.rs"
cp "$TEST" "$BACKUP_DIR/peer_wire.rs"

restore() {
  cp "$BACKUP_DIR/lib.rs" "$LIB"
  cp "$BACKUP_DIR/peer_wire.rs" "$TEST"
}
trap restore EXIT INT TERM

run_gate() {
  # $1 = test name, $2 = log file. Never piped: the exit code is the verdict.
  CARGO_TARGET_DIR="$ROOT/target-mutate" cargo test \
    --manifest-path "$ROOT/Cargo.toml" --test peer_wire "$1" \
    >"$2" 2>&1
  echo $?
}

apply() {
  # $1 = the field declaration to graft into MoveOffer
  # $2 = the literal field initialiser for the test's `MoveOffer {}`
  restore
  python3 - "$LIB" "$TEST" "$1" "$2" <<'PY'
import sys
lib_path, test_path, field, init = sys.argv[1:5]
lib = open(lib_path).read()
needle = "pub struct MoveOffer {}"
assert needle in lib, "positive control: MoveOffer's declaration moved"
# `MoveOffer` is an empty struct deriving `Copy` and `Default`, and a `String`
# field is compatible with neither. Dropping the two derives is part of the
# mutation rather than a workaround: a real edit adding a token field would hit
# the same two compile errors and answer them the same way.
derive = "#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]\n#[serde(rename_all = \"camelCase\")]\npub struct MoveOffer {}"
assert derive in lib, "positive control: MoveOffer's derive line moved"
lib = lib.replace(
    derive,
    "#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]\n"
    "#[serde(rename_all = \"camelCase\")]\n"
    "pub struct MoveOffer {\n    " + field + "\n}",
)
open(lib_path, "w").write(lib)

test = open(test_path).read()
needle = "MoveOffer {}"
assert needle in test, "positive control: the MoveOffer sample moved"
test = test.replace(needle, "MoveOffer { " + init + " }")
open(test_path, "w").write(test)
PY
}

probe() {
  local name="$1" field="$2" init="$3"
  apply "$field" "$init"
  # The control that matters: a mutant that does not COMPILE makes every gate
  # exit non-zero for a reason that has nothing to do with the gate. Measured
  # on the first run of this script: `MoveOffer` derives `Copy`, a `String`
  # field broke it, and all three probes "caught" a build error.
  local built
  CARGO_TARGET_DIR="$ROOT/target-mutate" cargo check --manifest-path "$ROOT/Cargo.toml" \
    --all-targets >"$LOG_DIR/mutate-$STAMP-$name-build.log" 2>&1
  built=$?
  if [ "$built" != "0" ]; then
    echo "probe=$name INVALID: the mutant does not compile (see $LOG_DIR/mutate-$STAMP-$name-build.log)"
    FAILED=1
    return
  fi
  local deny allow
  deny=$(run_gate wire_has_no_credential_field "$LOG_DIR/mutate-$STAMP-$name-deny.log")
  allow=$(run_gate every_wire_type_serializes_only_allowlisted_keys "$LOG_DIR/mutate-$STAMP-$name-allow.log")
  echo "probe=$name denylist_exit=$deny allowlist_exit=$allow"
  if [ "$allow" = "0" ]; then
    echo "FAIL: the allowlist gate stayed green while $name was on the wire"
    FAILED=1
  fi
}

FAILED=0
probe join_key_b64 'pub join_key_b64: String,' 'join_key_b64: "AAAA".to_string()'
probe access_token_jwt 'pub access_token_jwt: String,' 'access_token_jwt: "eyJhbGci".to_string()'
probe serde_rename '#[serde(rename = "accessToken")] pub note: String,' 'note: "eyJhbGci".to_string()'

restore
green=$(run_gate every_wire_type_serializes_only_allowlisted_keys "$LOG_DIR/mutate-$STAMP-restored.log")
echo "restored_allowlist_exit=$green"
if [ "$green" != "0" ]; then
  echo "FAIL: the tree did not come back green after the mutations"
  FAILED=1
fi

if [ "$FAILED" = "0" ]; then
  echo "ALL PROBES CAUGHT"
else
  echo "MUTATION TEST FAILED"
fi
exit "$FAILED"
