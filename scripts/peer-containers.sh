#!/usr/bin/env bash
# peer-containers.sh: run the peer mesh against real network shapes, in small
# containers, and print one greppable line per assertion.
#
#   scripts/peer-containers.sh                    # every scenario, serially
#   scripts/peer-containers.sh all                # the same
#   scripts/peer-containers.sh nat-upnp-mapping   # one scenario by name
#   scripts/peer-containers.sh --list             # what there is to run
#   scripts/peer-containers.sh nat-no-mapping --keep  # leave it standing
#   scripts/peer-containers.sh --netlab lan-discover-and-pair  # namespaces, not compose
#   scripts/peer-containers.sh --native all       # namespaces, no Docker, Linux root only
#
# --netlab runs the same scenario file inside one Linux network-namespace lab
# per scenario, instead of a compose project of separate containers: one
# `docker run -d --network none --cap-add NET_ADMIN --cap-add SYS_ADMIN`, one
# `docker exec` for the whole scenario, `docker rm -f` unless --keep. This is
# the control the compose path stays default until every scenario is green
# under both: running the same file two ways is the positive control that a
# netlab green is a real green and not a lab that fails to exercise anything.
#
# --native is --netlab with the container taken out: one `unshare --net
# --mount --pid --fork --mount-proc` per scenario instead of one `docker run`
# plus `docker exec`, no image build, the Docker daemon asked for nothing.
# Linux and root only. The three namespaces do what the container did: a
# private network, a private mount table so /lab and /lab-src still mean what
# tests/containers/lib/netlab.sh and every scenario hardcode them to mean, and
# a private pid table so a scenario that dies mid-way leaves nothing running.
# `--keep` is refused with --native: a namespace dies with the process that
# holds it, so there is nothing to keep.
#
# Measured on ubuntu-latest, 2026-09-20 (run 35533424004): 9 of 10 scenarios
# passed. nat-upnp-mapping did not: miniupnpd refused AddPortMapping with
# UPnP error 501 (ActionFailed) under a bare `unshare` namespace, on the same
# commit that scenario passes 10/10 under --netlab's Docker container. That
# is the one behaviour netlab-design.md section 10 already named as reasoned
# rather than observed ("miniupnpd against iptables-legacy in a namespace"),
# now observed and red, not yet root-caused. --native is a local,
# Linux-and-root option with this one known gap. Neither mode runs in CI:
# the netlab job was removed on 2026-09-24 because it no longer fit its time
# limit, so these scenarios run only when someone runs them by hand.
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
NETLAB=0
NATIVE=0
LIST=0
WANTED=""
for arg in "$@"; do
  case "$arg" in
    --keep) KEEP=1 ;;
    --netlab) NETLAB=1 ;;
    --native) NATIVE=1 ;;
    --list) LIST=1 ;;
    -h|--help) sed -n '2,57p' "${BASH_SOURCE[0]}"; exit 0 ;;
    all) WANTED="" ;;
    -*) echo "unknown flag: $arg" >&2; exit 2 ;;
    *) WANTED="$arg" ;;
  esac
done

# A namespace dies with the process that holds it, so a --keep run under
# --native would leave nothing to inspect; the flag pair is refused rather
# than printing a "kept" line for something already gone.
if [ "$NATIVE" -eq 1 ] && [ "$KEEP" -eq 1 ]; then
  echo "runner: FAIL: --native --keep: a namespace dies with the process that holds it, so there is nothing to keep" >&2
  exit 2
fi

if [ "$NATIVE" -eq 1 ]; then
  if [ "$(uname -s)" != "Linux" ]; then
    echo "runner: FAIL: --native needs Linux, this runner reports $(uname -s)" >&2
    exit 2
  fi
  if [ "$(id -u)" -ne 0 ]; then
    echo "runner: FAIL: --native needs root (unshare --net --mount --pid needs it), this shell is uid $(id -u)" >&2
    exit 2
  fi
fi

list_scenarios() {
  for file in "$SCENARIO_DIR"/*.sh; do
    basename "$file" .sh
  done
}

# Under compose --list is a directory read, so it costs nothing and needs no
# image. Under --netlab, --list also builds the image, because "the image
# builds and lists the scenarios" is the one thing phase 1 has to prove before
# any scenario runs under it. Under --native there is no image to prove, so
# --list is the same directory read as compose.
if [ "$LIST" -eq 1 ] && [ "$NETLAB" -eq 0 ] && [ "$NATIVE" -eq 0 ]; then
  list_scenarios
  exit 0
fi
if [ "$LIST" -eq 1 ] && [ "$NATIVE" -eq 1 ]; then
  list_scenarios
  exit 0
fi

# The tcr binary a native run execs inside each namespace. Never assumed at
# $ROOT/target/release/tcr: CARGO_TARGET_DIR redirects cargo's output in many
# environments here (CONTRIBUTING.md "Finding the binary you just built"),
# and trusting the default path silently execs a stale binary instead.
native_tcr_bin() {
  if [ -n "${NATIVE_TCR_BIN:-}" ]; then
    printf '%s\n' "$NATIVE_TCR_BIN"
    return
  fi
  dir=""
  if [ -n "${CARGO_TARGET_DIR:-}" ]; then
    dir="$CARGO_TARGET_DIR"
  elif command -v cargo >/dev/null 2>&1 && command -v jq >/dev/null 2>&1; then
    dir="$(cd "$ROOT" && cargo metadata --format-version 1 --no-deps 2>/dev/null | jq -r .target_directory 2>/dev/null || true)"
  fi
  [ -n "$dir" ] || dir="$ROOT/target"
  printf '%s/release/tcr\n' "$dir"
}

# The tools Dockerfile.netlab:16 installs into the image, checked on the host
# PATH instead, one FAIL line per tool missing so a runner that lacks one
# names it rather than failing three layers down inside a scenario.
NATIVE_TOOLS="ip iptables iptables-legacy miniupnpd python3 unshare"
native_preflight() {
  missing=0
  for tool in $NATIVE_TOOLS; do
    if ! command -v "$tool" >/dev/null 2>&1; then
      echo "runner: FAIL: --native needs $tool on PATH, which Dockerfile.netlab:16 installs into the image" >&2
      missing=1
    fi
  done
  if [ ! -x "$NATIVE_TCR_BIN" ]; then
    echo "runner: FAIL: --native needs a built tcr at $NATIVE_TCR_BIN (cargo build --release --locked --bin tcr, the flags Dockerfile.tcr:31 uses), or set NATIVE_TCR_BIN" >&2
    missing=1
  fi
  [ "$missing" -eq 0 ]
}

if [ "$NATIVE" -eq 0 ]; then
  if ! command -v "$DOCKER" >/dev/null 2>&1; then
    echo "runner: FAIL: no docker on PATH (set DOCKER=/path/to/docker)" >&2
    exit 2
  fi
  # The scenarios read `tcr peer ls --json` through tests/containers/lib/peer-read.py.
  # Under compose that reader runs on the host, so python3 on the host's PATH is
  # refused up front. Under --netlab the python3 that matters is the one baked
  # into the lab image, and a scenario runs whole inside one `docker exec`, so
  # the host check is skipped: the host still needs Docker, nothing else.
  if [ "$NETLAB" -eq 0 ] && ! command -v python3 >/dev/null 2>&1; then
    echo "runner: FAIL: no python3 on PATH, which the scenarios read the peer listing with" >&2
    exit 2
  fi
fi

if [ "$NATIVE" -eq 1 ]; then
  NATIVE_TCR_BIN="$(native_tcr_bin)"
  echo "== native preflight (tcr at $NATIVE_TCR_BIN)"
  native_preflight || exit 2
  NATIVE_TCR_DIR="$(dirname "$NATIVE_TCR_BIN")"
  # /lab-src and /lab have to exist as directories before a bind mount or a
  # tmpfs can target them; each unshare --mount below privatises whatever
  # gets mounted onto them, so the runner's own /lab-src and /lab, created
  # here, never see a scenario's writes.
  NATIVE_CREATED_LAB_SRC=0
  NATIVE_CREATED_LAB=0
  if [ ! -d /lab-src ]; then
    mkdir -p /lab-src
    NATIVE_CREATED_LAB_SRC=1
    echo "native: created /lab-src (bind-mount target for tests/containers)"
  fi
  if [ ! -d /lab ]; then
    mkdir -p /lab
    NATIVE_CREATED_LAB=1
    echo "native: created /lab (tmpfs target for the per-scenario lab tree)"
  fi
elif [ "$NETLAB" -eq 1 ]; then
  echo "== building the netlab image (CARGO_BUILD_JOBS=$JOBS)"
  "$DOCKER" build --build-arg "CARGO_BUILD_JOBS=$JOBS" \
    -f "$CONTAINERS/Dockerfile.tcr" -t tcr-harness/tcr:dev "$ROOT" || exit 1
  "$DOCKER" build -f "$CONTAINERS/Dockerfile.netlab" -t tcr-harness/netlab:dev "$ROOT" || exit 1
  if [ "$LIST" -eq 1 ]; then
    list_scenarios
    exit 0
  fi
else
  echo "== building the images (CARGO_BUILD_JOBS=$JOBS)"
  "$DOCKER" build --build-arg "CARGO_BUILD_JOBS=$JOBS" \
    -f "$CONTAINERS/Dockerfile.tcr" -t tcr-harness/tcr:dev "$ROOT" || exit 1
  "$DOCKER" build -f "$CONTAINERS/Dockerfile.router" -t tcr-harness/router:dev "$ROOT" || exit 1
fi

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
  echo "== $name"
  if [ "$NATIVE" -eq 1 ]; then
    # One `unshare --net --mount --pid --fork --mount-proc` per scenario, the
    # native equivalent of one `docker run --network none --cap-add NET_ADMIN
    # --cap-add SYS_ADMIN` plus `docker exec`: a private network, a private
    # mount table (native-inner.sh binds /lab-src and tmpfs's /lab and
    # /run/netns inside it), and a private pid table so a scenario that dies
    # mid-way cannot leave a tcr process behind on the runner, same as a
    # container's own pid namespace would.
    unshare --net --mount --pid --fork --mount-proc -- \
      "$CONTAINERS/lib/native-inner.sh" "$name" "$CONTAINERS" "$NATIVE_TCR_DIR"
    ran=$?
    if [ "$ran" -ne 0 ]; then
      status=1
    fi
  elif [ "$NETLAB" -eq 1 ]; then
    # One namespace lab per scenario, in one container: nothing here can reach
    # the host or the Docker daemon's own networking (--network none), and the
    # two capabilities are everything the sketch measured needing, never
    # --privileged. The default docker-default AppArmor profile (active on
    # ubuntu-latest, absent on OrbStack) denies the mount syscall `ip netns
    # add` needs even with SYS_ADMIN held, so apparmor=unconfined is what lets
    # the two capabilities actually do their job, not a third capability.
    container="netlab-$name"
    "$DOCKER" run -d --name "$container" --network none \
      --cap-add NET_ADMIN --cap-add SYS_ADMIN \
      --security-opt apparmor=unconfined \
      tcr-harness/netlab:dev >/dev/null || { echo "$name: FAIL: netlab container did not start" >&2; status=1; continue; }
    "$DOCKER" exec -e "NETLAB=1" "$container" "/lab-src/scenarios/$name.sh"
    ran=$?
    if [ "$ran" -ne 0 ]; then
      status=1
    fi
    if [ "$KEEP" -eq 1 ]; then
      echo "$name: kept: docker exec $container /lab-src/netlab ps"
    else
      "$DOCKER" rm -f "$container" >/dev/null 2>&1
    fi
  else
    # A project per scenario, so one scenario's leftovers can never be another's
    # starting state, and a --keep run can be inspected by name.
    project="tcr-peers-$name"
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
  fi
done

if [ "$NATIVE" -eq 1 ]; then
  [ "$NATIVE_CREATED_LAB_SRC" -eq 1 ] && rmdir /lab-src 2>/dev/null
  [ "$NATIVE_CREATED_LAB" -eq 1 ] && rmdir /lab 2>/dev/null
  true
fi

if [ "$status" -eq 0 ]; then
  echo "== PASS: every assertion held"
else
  echo "== FAIL: grep the FAIL lines above" >&2
fi
exit "$status"
