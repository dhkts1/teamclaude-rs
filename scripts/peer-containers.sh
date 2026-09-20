#!/usr/bin/env bash
# peer-containers.sh: run the peer mesh against real network shapes, in small
# containers, and print one greppable line per assertion.
#
#   scripts/peer-containers.sh                    # every scenario, serially
#   scripts/peer-containers.sh all                # the same
#   scripts/peer-containers.sh nat-upnp-mapping   # one scenario by name
#   scripts/peer-containers.sh --list             # what there is to run
#   scripts/peer-containers.sh nat-no-mapping --keep  # leave it standing
#
# This is the container half of the peer end-to-end. The in-process half,
# `scripts/peer-e2e-local.sh` over `tests/peer_e2e.rs`, stays the fast gate and
# this does not replace it: what lives here is every fact that needs a network
# shape one host cannot make, a NAT that refuses to map, a friend on another
# network, an overlay address ranked ahead of a LAN one.
#
# Safety, which is the same in every scenario and is not per-scenario
# discretion: every container's HOME is a scratch directory the image creates,
# every account is obviously fake, the upstream is a stub in the compose
# project, and nothing here reads or writes the operator's config directory.
# The live proxy is on the host's loopback, which is not reachable from a
# container's network namespace at all, and no scenario publishes a port on it.
#
# Each scenario is one script under tests/containers/scenarios/. It prints
# `<name>: PASS|FAIL: <fact>` lines and exits 0 or 1. This runner builds the
# images once, runs the scripts one at a time with a compose project of their
# own, and tears each project down unless --keep.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CONTAINERS="$ROOT/tests/containers"
SCENARIO_DIR="$CONTAINERS/scenarios"
DOCKER="${DOCKER:-docker}"
# Three, for the same reason the host build uses three: a container build that
# takes every core starves whatever else the machine is doing.
JOBS="${CARGO_BUILD_JOBS:-3}"

KEEP=0
WANTED=""
for arg in "$@"; do
  case "$arg" in
    --keep) KEEP=1 ;;
    --list)
      for file in "$SCENARIO_DIR"/*.sh; do
        basename "$file" .sh
      done
      exit 0
      ;;
    -h|--help) sed -n '2,30p' "${BASH_SOURCE[0]}"; exit 0 ;;
    all) WANTED="" ;;
    -*) echo "unknown flag: $arg" >&2; exit 2 ;;
    *) WANTED="$arg" ;;
  esac
done

if ! command -v "$DOCKER" >/dev/null 2>&1; then
  echo "runner: FAIL: no docker on PATH (set DOCKER=/path/to/docker)" >&2
  exit 2
fi

echo "== building the images (CARGO_BUILD_JOBS=$JOBS)"
"$DOCKER" build --build-arg "CARGO_BUILD_JOBS=$JOBS" \
  -f "$CONTAINERS/Dockerfile.tcr" -t tcr-harness/tcr:dev "$ROOT" || exit 1
"$DOCKER" build -f "$CONTAINERS/Dockerfile.router" -t tcr-harness/router:dev "$ROOT" || exit 1

scenarios=()
if [ -n "$WANTED" ]; then
  if [ ! -x "$SCENARIO_DIR/$WANTED.sh" ]; then
    echo "runner: FAIL: no scenario named $WANTED (try --list)" >&2
    exit 2
  fi
  scenarios=("$SCENARIO_DIR/$WANTED.sh")
else
  for file in "$SCENARIO_DIR"/*.sh; do
    scenarios+=("$file")
  done
fi

status=0
for script in "${scenarios[@]}"; do
  name="$(basename "$script" .sh)"
  # A project per scenario, so one scenario's leftovers can never be another's
  # starting state, and a --keep run can be inspected by name.
  project="tcr-peers-$name"
  echo "== $name"
  COMPOSE_PROJECT="$project" \
  COMPOSE_FILE="$CONTAINERS/compose.yml" \
  DOCKER="$DOCKER" \
    "$script"
  ran=$?
  if [ "$ran" -ne 0 ]; then
    status=1
  fi
  if [ "$KEEP" -eq 1 ]; then
    echo "$name: kept: docker compose -p $project ps"
  else
    "$DOCKER" compose -p "$project" -f "$CONTAINERS/compose.yml" down -v --remove-orphans >/dev/null 2>&1
  fi
done

if [ "$status" -eq 0 ]; then
  echo "== PASS: every assertion held"
else
  echo "== FAIL: grep the FAIL lines above" >&2
fi
exit "$status"
