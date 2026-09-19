#!/usr/bin/env bash
# peer-e2e-local.sh: run the two-process peer end-to-end on this machine and
# print one line per step.
#
# The path itself lives in `tests/peer_e2e.rs`, which spawns the `tcr` this
# build produced twice (`CARGO_BIN_EXE_tcr`) against two scratch HOMEs and
# drives it through the CLI and the peer port only. This script is the operator
# front door to that test: it builds the binary the test will spawn, runs the
# test with its step trace visible, and prints the trace.
#
# One implementation, not two. An earlier draft of this script re-drove the
# whole path in bash beside the Rust test; two drivers for one path is two
# things to keep in step, and the shell copy is the one nobody runs in CI.
#
#   scripts/peer-e2e-local.sh                  # build, then run every test (N=2 ring)
#   scripts/peer-e2e-local.sh --nodes 3        # same, with the ring at N=3
#   scripts/peer-e2e-local.sh --nodes 3 --moved  # also the moved-peer legs
#   scripts/peer-e2e-local.sh --nodes 3 --undialable  # the reverse-carry legs
#
# `--nodes N` (2..=5) sets `TCR_E2E_NODES`, which only `n_macs_pair_in_a_ring`
# reads: it boots N `tcr` processes and pairs them in a ring instead of the
# default two. Every other test in the file (the two-process borrow path, the
# chained-borrow and inflight-contention tests, which always stand up their
# own fixed three-Mac fleets) runs exactly the same regardless of N.
#
# `--moved` adds `--include-ignored` and narrows the run to two name filters,
# `moved` and `forward`, one process per filter: item 1 (a moved peer) and
# item 2 (a forwarded borrow). Both match now.
# `a_moved_peer_is_reached_through_a_refreshed_hello_endpoint` runs against the
# `tcr peer hello` verb, and
# `a_forwarded_borrow_reaches_the_lender_through_a_third_mac` stands up three
# Macs, strips the lender's addresses out of the borrower's peers file, and
# asserts the request is served anyway on the lender's own credential with
# `peer forward: carried` in the carrier's log. It was written the day the
# client half of a forward landed (`tunnel::open_forward_to`,
# `serve::dial_peer_reaching`); before that the filter reported "0 tests run",
# which is what this flag does with a leg that has no test rather than a false
# pass. `--include-ignored` stays because it costs nothing and is what picks up
# a leg that is still ignored.
#
# The borrow leg asserts the SERVED 200 unconditionally (LEASE-WIRE's
# `listener::serve_control` `Control::LeaseRequest` arm, `src/peer/listener.rs`):
# until that arm is in this tree, the test is red there and says so.
#
# Safety, in the test and therefore here: every proxy binds `--port 0`, so the
# live proxy on 127.0.0.1:3456 is never probed, signalled or connected to;
# every HOME is a tempdir, so nothing reads the operator's config directory
# or cache directory; every
# account is obviously fake. Honours CARGO_TARGET_DIR.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MANIFEST="$ROOT/Cargo.toml"

NODES=""
MOVED=0
UNDIALABLE=0
while [ $# -gt 0 ]; do
  case "$1" in
    -h|--help) sed -n '2,32p' "${BASH_SOURCE[0]}"; exit 0 ;;
    --nodes)
      NODES="${2:-}"
      if [ -z "$NODES" ]; then
        echo "--nodes needs a value (2..=5)" >&2
        exit 2
      fi
      case "$NODES" in
        2|3|4|5) ;;
        *) echo "--nodes must be 2..=5, got $NODES" >&2; exit 2 ;;
      esac
      shift 2
      ;;
    --moved)
      MOVED=1
      shift
      ;;
    --undialable)
      UNDIALABLE=1
      shift
      ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

echo "== building the tcr the test will spawn"
cargo build --manifest-path "$MANIFEST" --bin tcr || exit 1

if [ -n "$NODES" ]; then
  echo "== the N-process peer end-to-end (TCR_E2E_NODES=$NODES)"
else
  echo "== the peer end-to-end"
fi
if [ "$MOVED" -eq 1 ]; then
  echo "== also items 1 and 2 (moved peer, forwarded borrow): --include-ignored, one filter each"
fi
if [ "$UNDIALABLE" -eq 1 ]; then
  echo "== also the reverse carry: the undialable leg and the forwarder's own half"
fi

# `--test-threads=1` so the STEP lines of the tests do not interleave, and
# `--nocapture` because those lines are the point of running this by hand.
CARGO_ARGS=(test --manifest-path "$MANIFEST" --test peer_e2e)

run_suite() {
  # $1: an extra test-name filter, or empty for the whole file.
  local filter="$1"
  local test_args=(--nocapture --test-threads=1)
  if [ -n "$filter" ]; then
    test_args=(--include-ignored "$filter" --nocapture --test-threads=1)
  fi
  if [ -n "$NODES" ]; then
    TCR_E2E_NODES="$NODES" cargo "${CARGO_ARGS[@]}" -- "${test_args[@]}"
  else
    cargo "${CARGO_ARGS[@]}" -- "${test_args[@]}"
  fi
}

status=0
run_suite "" || status=$?
if [ "$MOVED" -eq 1 ] && [ "$status" -eq 0 ]; then
  for filter in moved forward; do
    echo "== moved-peer item: filter '$filter'"
    run_suite "$filter" || { status=$?; break; }
  done
fi

if [ "$UNDIALABLE" -eq 1 ] && [ "$status" -eq 0 ]; then
  echo "== reverse carry: the three-process undialable leg"
  run_suite undialable || status=$?
  if [ "$status" -eq 0 ]; then
    echo "== reverse carry: the forwarder's own half, in process"
    cargo test --manifest-path "$MANIFEST" --test peer_forward -- \
      --nocapture --test-threads=1 carrier || status=$?
  fi
  if [ "$status" -eq 0 ]; then
    echo "== PASS --undialable: a Mac nobody can dial is reached over the carrier it parked, and a borrow with no carrier and no address ends rather than hangs"
  fi
fi

if [ "$status" -eq 0 ]; then
  echo "== PASS: the whole path ran on this machine"
else
  echo "== FAIL: see the step the trace above stops at (exit $status)" >&2
fi
exit "$status"
