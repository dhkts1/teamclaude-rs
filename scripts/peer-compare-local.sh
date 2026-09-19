#!/usr/bin/env bash
# peer-compare-local.sh: three peers-vs-no-peers comparisons, each a number
# beside the command that produced it.
#
# 1. Feature off is a no-op: this worktree's test-name and pass/fail sets
#    diffed against a throwaway comparison worktree built at the pre-peers
#    merge-base (`PEER_COMPARE_WORKTREE`, default a sibling worktree named
#    `lan-p2p-main-cmp`). A test present and green at the
#    merge-base that is red here is printed as `finding: <name>`.
# 2. Latency: runs `measure_latency_local_vs_borrowed` in
#    `tests/peer_e2e.rs`: 50 requests served locally on the lender versus 50
#    borrowed through the lender, through the same two-process harness
#    `scripts/peer-e2e-local.sh` drives.
# 3. Boot cost: runs `measure_boot_cost_with_and_without_peers_file`: ten
#    proxy boots with a `tcr-peers.json` present and ten without, keyed on
#    the ready log line the server prints (`src/main.rs:3732`).
#
# Every line this script cares about is `measure: <name> <value> <unit>` or
# `finding: <text>`: grep for either. The numbers are printed here only,
# never written into a committed doc.
#
# Safety, same as tests/peer_e2e.rs and scripts/peer-e2e-local.sh: every
# proxy either comparison boots binds --port 0 with a scratch HOME; the live
# proxy on 127.0.0.1:3456 is never probed, signalled or connected to.
# Honours CARGO_TARGET_DIR for this worktree's build; the comparison
# worktree in comparison 1 gets its own target dir
# (`PEER_COMPARE_TARGET_DIR`, default `<comparison worktree>/target-cmp`) so
# the two builds never share or corrupt one cache.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MANIFEST="$ROOT/Cargo.toml"
CMP_WORKTREE="${PEER_COMPARE_WORKTREE:-$HOME/worktrees/lan-p2p-main-cmp}"
CMP_MANIFEST="$CMP_WORKTREE/Cargo.toml"
CMP_TARGET_DIR="${PEER_COMPARE_TARGET_DIR:-$CMP_WORKTREE/target-cmp}"

case "${1:-}" in
  "") ;;
  -h|--help) sed -n '2,29p' "${BASH_SOURCE[0]}"; exit 0 ;;
  *) echo "unknown argument: $1" >&2; exit 2 ;;
esac

: "${CARGO_TARGET_DIR:?set CARGO_TARGET_DIR to this worktree's own build output dir before running}"

LOGDIR="${TMPDIR:-/tmp}/peer-compare-$$"
mkdir -p "$LOGDIR"
echo "logs: $LOGDIR"

overall_status=0

echo "== comparison 1: feature off is a no-op =="
if [ ! -f "$CMP_MANIFEST" ]; then
  echo "SKIP comparison 1: no comparison worktree at $CMP_WORKTREE (set PEER_COMPARE_WORKTREE)" >&2
else
  echo "-- listing test names --"
  CARGO_TARGET_DIR="$CMP_TARGET_DIR" cargo test --manifest-path "$CMP_MANIFEST" --workspace --no-fail-fast -- --list \
    > "$LOGDIR/main-list-raw.txt" 2>&1
  cargo test --manifest-path "$MANIFEST" --workspace --no-fail-fast -- --list \
    > "$LOGDIR/branch-list-raw.txt" 2>&1
  grep -E ': test$' "$LOGDIR/main-list-raw.txt" | sed -E 's/: test$//' | sort -u > "$LOGDIR/main-list.txt"
  grep -E ': test$' "$LOGDIR/branch-list-raw.txt" | sed -E 's/: test$//' | sort -u > "$LOGDIR/branch-list.txt"
  removed=$(comm -23 "$LOGDIR/main-list.txt" "$LOGDIR/branch-list.txt" | wc -l | tr -d ' ')
  added=$(comm -13 "$LOGDIR/main-list.txt" "$LOGDIR/branch-list.txt" | wc -l | tr -d ' ')
  echo "measure: comparison1_names_removed $removed count"
  echo "measure: comparison1_names_added $added count"
  if [ "$removed" -gt 0 ]; then
    echo "finding: $removed test name(s) present at the merge-base disappeared on this branch:"
    comm -23 "$LOGDIR/main-list.txt" "$LOGDIR/branch-list.txt" | sed 's/^/finding:   /'
    overall_status=1
  fi

  echo "-- running the full suite on both sides (this takes a while) --"
  CARGO_TARGET_DIR="$CMP_TARGET_DIR" cargo test --manifest-path "$CMP_MANIFEST" --workspace --no-fail-fast \
    > "$LOGDIR/main-run.log" 2>&1
  cargo test --manifest-path "$MANIFEST" --workspace --no-fail-fast \
    > "$LOGDIR/branch-run.log" 2>&1

  grep -E "^test [a-zA-Z0-9_:]+ \.\.\. (ok|FAILED)" "$LOGDIR/main-run.log" \
    | sed -E 's/^test ([a-zA-Z0-9_:]+) \.\.\. (ok|FAILED)/\1: \2/' | sort -u > "$LOGDIR/main-pass.txt"
  grep -E "^test [a-zA-Z0-9_:]+ \.\.\. (ok|FAILED)" "$LOGDIR/branch-run.log" \
    | sed -E 's/^test ([a-zA-Z0-9_:]+) \.\.\. (ok|FAILED)/\1: \2/' | sort -u > "$LOGDIR/branch-pass.txt"

  # comm -3: lines that differ between the two sorted "name: status" files.
  # A shared name whose status flipped shows up here; so does every new
  # branch-only test. Intersecting the diffed names with the merge-base's own
  # test-name list keeps only the former.
  comm -3 "$LOGDIR/main-pass.txt" "$LOGDIR/branch-pass.txt" \
    | sed -E 's/^\t?([a-zA-Z0-9_:]+): (ok|FAILED)/\1/' | sort -u > "$LOGDIR/diff-names.txt"
  comm -12 "$LOGDIR/diff-names.txt" "$LOGDIR/main-list.txt" > "$LOGDIR/flipped-names.txt"
  flipped=$(wc -l < "$LOGDIR/flipped-names.txt" | tr -d ' ')
  echo "measure: comparison1_status_flips $flipped count"
  if [ "$flipped" -gt 0 ]; then
    while IFS= read -r name; do
      [ -z "$name" ] && continue
      echo "finding: $name existed and passed at the pre-peers merge-base but is red on this branch"
      grep -A6 "^test $name \.\.\. FAILED" "$LOGDIR/branch-run.log" | sed 's/^/finding:   /'
    done < "$LOGDIR/flipped-names.txt"
    overall_status=1
  fi
fi

echo "== comparison 2: latency, local vs borrowed =="
cargo test --manifest-path "$MANIFEST" --test peer_e2e measure_latency_local_vs_borrowed \
  -- --ignored --nocapture --test-threads=1 > "$LOGDIR/latency.log" 2>&1
latency_status=$?
grep -E '^(STEP|measure):' "$LOGDIR/latency.log"
[ "$latency_status" -eq 0 ] || { echo "the latency measurement test itself failed (exit $latency_status); log: $LOGDIR/latency.log" >&2; overall_status=1; }

echo "== comparison 3: boot cost, with and without a peers file =="
cargo test --manifest-path "$MANIFEST" --test peer_e2e measure_boot_cost_with_and_without_peers_file \
  -- --ignored --nocapture --test-threads=1 > "$LOGDIR/boot.log" 2>&1
boot_status=$?
grep -E '^(STEP|measure):' "$LOGDIR/boot.log"
[ "$boot_status" -eq 0 ] || { echo "the boot-cost measurement test itself failed (exit $boot_status); log: $LOGDIR/boot.log" >&2; overall_status=1; }

exit "$overall_status"
