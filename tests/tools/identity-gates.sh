#!/opt/homebrew/bin/bash
# Runs fmt, check, clippy and the workspace test suite, each redirected to
# its own log and never piped, with a `DONE <name> exit=<code>` line
# appended to /tmp/lan-p2p/identity-gates.log so a waiter can block on a
# marker the run itself emits.
set -uo pipefail
# The repository this script belongs to, derived from the script's own location:
# a hardcoded absolute path would be one reader's home directory in a PUBLIC repo,
# and wrong in every other checkout.
ROOT=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
export CARGO_TARGET_DIR="$ROOT/target-check"
LOG=/tmp/lan-p2p/identity-gates.log
mkdir -p /tmp/lan-p2p

run() {
  local name="$1"
  shift
  "$@" >"/tmp/lan-p2p/identity-gate-$name.log" 2>&1
  echo "DONE $name exit=$?" >>"$LOG"
}

run fmt cargo fmt --manifest-path "$ROOT/Cargo.toml" --all --check
run check cargo check --manifest-path "$ROOT/Cargo.toml" --workspace
run clippy cargo clippy --manifest-path "$ROOT/Cargo.toml" --all-targets --locked
run test cargo test --manifest-path "$ROOT/Cargo.toml" --workspace
echo "DONE all" >>"$LOG"
