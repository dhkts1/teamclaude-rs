#!/bin/sh
# The verbs every scenario repeats, over namespaces instead of containers.
#
# Same names and same meanings as tests/containers/lib/compose.sh: a scenario
# says `up node-a1 node-a2`, `nx node-a1 tcr peer ls --json`, `pair_nodes a b
# addr`, and never learns which harness it is running under.
#
# Two translations make that true and both are here rather than in the
# scenarios:
#
#   1. `/scratch` means THIS node's scratch. Under compose every container had
#      its own /scratch at the same path; a namespace shares the container's
#      filesystem, so `nx` rewrites a leading /scratch in every argument to
#      /lab/<node>. A scenario keeps saying `--peers "$PEERS"`.
#   2. A boot log is a local file, not a `docker exec grep`. `wait_for_log`
#      polls /lab/<node>/boot.log directly, which is why it can poll ten times
#      a second instead of once.
: "${SCENARIO:?a scenario must set SCENARIO before sourcing netlab.sh}"
: "${TOPOLOGY:?a scenario must set TOPOLOGY to its topology file}"

LAB=/lab
SRC=/lab-src
NETLAB_BIN="$SRC/netlab"
LIB="$SRC/lib"
PEERS=/scratch/.config/tcr-peers.json
DRIVER="$SRC/node-driver.sh"
# shellcheck disable=SC2034 # read by the scenario that sourced this, not here
LISTEN_PORT=7755

SCRATCH="${TMPDIR:-/tmp}/tcr-netlab-$SCENARIO-$$"
mkdir -p "$SCRATCH"
cleanup_scratch() { rm -rf "$SCRATCH"; }
trap cleanup_scratch EXIT

# Rewrite a leading /scratch to this node's own scratch. Newline-separated:
# none of a wire id, an address, an instance code or a path in this harness
# ever carries one, and a plain `read` loop is what lets nx() rebuild its argv
# without a pipe (below).
rewrite() {
  node="$1"
  shift
  for arg in "$@"; do
    case "$arg" in
      /scratch/*) printf '%s\n' "$LAB/$node/${arg#/scratch/}" ;;
      *) printf '%s\n' "$arg" ;;
    esac
  done
}

nx() {
  node="$1"
  shift
  # Rebuilt through a file and a `read` loop, never a pipe into xargs: xargs
  # gives the command it execs a stdin of its own (busybox: /dev/null, `-a
  # FILE` or not), not the caller's, and `tcr peer moved open --stdin` reads a
  # link off exactly the stdin a scenario piped into `nxi`. Piping the argv
  # through anything silently ate every link a scenario ever sent.
  #
  # $$ alone is not a unique enough filename: nxd backgrounds a call that can
  # still be running, argfile and all, when the next nx() for the same
  # scenario starts.
  argfile="$(mktemp "$SCRATCH/.nx-argv-XXXXXX")"
  rewrite "$node" "$@" > "$argfile"
  set --
  while IFS= read -r arg; do
    set -- "$@" "$arg"
  done < "$argfile"
  rm -f "$argfile"
  "$NETLAB_BIN" nx "$node" "$@"
}

# Under compose these were `docker exec -i` and `-d`. A namespace needs neither
# flag: standard input is already this shell's, and `&` already detaches.
nxi() { nx "$@"; }
nxd() { nx "$@" >/dev/null 2>&1 & }

up() {
  if [ ! -f "$LAB/.plan" ]; then
    "$NETLAB_BIN" up "$TOPOLOGY" || { fail "netlab up $TOPOLOGY refused"; return 1; }
  fi
  for node in "$@"; do
    if ! wait_for_log "$node" "peer listener up" 90 >/dev/null; then
      fail "$node: no 'peer listener up' in its boot log within 90s"
      return 1
    fi
  done
  return 0
}

wait_for_log() {
  node="$1"
  pattern="$2"
  seconds="${3:-60}"
  i=0
  while [ "$i" -lt "$((seconds * 10))" ]; do
    line="$(grep -hE "$pattern" "$LAB/$node/boot.log" 2>/dev/null | tail -1)"
    if [ -n "$line" ]; then
      echo "$line"
      return 0
    fi
    sleep 0.1
    i=$((i + 1))
  done
  return 1
}

# A router is a namespace whose log is a file like every other, so this is
# `wait_for_log` under the name the router scenarios already call.
wait_router_log() { wait_for_log "$@"; }

up_with_routers() {
  routers=""
  nodes=""
  side=routers
  for arg in "$@"; do
    if [ "$arg" = "--" ]; then side=nodes; continue; fi
    if [ "$side" = routers ]; then routers="$routers $arg"; else nodes="$nodes $arg"; fi
  done
  if [ ! -f "$LAB/.plan" ]; then
    "$NETLAB_BIN" up "$TOPOLOGY" || { fail "netlab up $TOPOLOGY refused"; return 1; }
  fi
  for router in $routers; do
    wait_router_log "$router" "^router: up:" 60 >/dev/null || { fail "$router: no 'router: up:' within 60s"; return 1; }
  done
  for node in $nodes; do
    wait_for_log "$node" "peer listener up" 90 >/dev/null || { fail "$node: no 'peer listener up' within 90s"; return 1; }
  done
  return 0
}

# The mDNS observer: a namespace on the LAN rather than a one-shot compose run.
observe() {
  node="${1:-observer}"
  window="${2:-10}"
  "$NETLAB_BIN" nx "$node" python3 "$SRC/mdns-observe.py" "$window" "$(cat "$LAB/$node/addr")"
}

own_short_id() { nx "$1" tcr peer id --peers "$PEERS" | tr -d '\r\n'; }

pinned_wire_id() {
  ids="$(python3 "$LIB/peer-read.py" nodes < "$1" 2>/dev/null || true)"
  if [ "$(echo "$ids" | grep -c .)" != "1" ]; then
    return 1
  fi
  echo "$ids" | tr -d '\r\n'
}

short_of() { printf 'tcr-%s\n' "$(printf '%s' "$1" | cut -c1-10)"; }

ls_json() {
  node="$1"
  out="$2"
  nx "$node" tcr peer ls --peers "$PEERS" --json > "$out" 2>/dev/null
}

# pair_nodes <initiator> <responder> <responder-addr>
#
# The whole six-digit pairing, driven from outside: knock, wait for the request
# to queue at the responder, accept it there, read the digits off BOTH screens,
# confirm they are the same, and answer. Copied from compose.sh's version with
# one edit: the pending poll's `sleep 1` is `sleep 0.5`, because a namespace's
# `nx` is cheap enough that the poll no longer needs to be gentle.
pair_nodes() {
  initiator="$1"
  responder="$2"
  addr="$3"

  if ! nxd "$initiator" "$DRIVER" pair-start "$addr"; then
    fail "$initiator: could not start a pairing with $addr"
    return 1
  fi

  instance="$(nx "$initiator" "$DRIVER" pair-instance | tr -d '\r\n')"
  if [ -z "$instance" ]; then
    fail "$initiator: the knock at $addr announced no instance id"
    return 1
  fi
  pass "$initiator knocked at $addr as instance $instance"

  queued=""
  i=0
  while [ "$i" -lt 60 ]; do
    if nx "$responder" tcr peer pending --peers "$PEERS" --json 2>/dev/null | grep -q "$instance"; then
      queued=yes
      break
    fi
    sleep 0.5
    i=$((i + 1))
  done
  if [ -z "$queued" ]; then
    fail "$responder: no pending row for instance $instance within 30s"
    return 1
  fi
  pass "$responder queued one request naming instance $instance"

  accepted="$(nx "$responder" tcr peer accept "$instance" --peers "$PEERS" 2>&1)"
  case "$accepted" in
    *"instance=$instance"*) pass "$responder accepted instance $instance" ;;
    *) fail "$responder: accept did not name the instance it approved: $accepted"; return 1 ;;
  esac

  initiator_code="$(nx "$initiator" "$DRIVER" pair-code | tr -d '\r\n')"
  responder_line="$(wait_for_log "$responder" "peer pairing: compare these six digits" 60)"
  responder_code="$(echo "$responder_line" | sed -n 's/.*code=\([0-9][0-9]*\).*/\1/p')"
  if [ -z "$initiator_code" ] || [ -z "$responder_code" ]; then
    fail "$initiator/$responder: a six-digit compare with only one screen ($initiator_code/$responder_code)"
    return 1
  fi
  if [ "$initiator_code" = "$responder_code" ]; then
    pass "$initiator and $responder show the same six digits ($initiator_code)"
  else
    fail "$initiator shows $initiator_code and $responder shows $responder_code"
    return 1
  fi

  nx "$initiator" "$DRIVER" pair-answer "$initiator_code" >/dev/null
  outcome="$(nx "$initiator" "$DRIVER" pair-wait 2>&1)"
  case "$outcome" in
    *'"event":"trusted"'*) pass "$initiator pinned $responder after the compare" ;;
    *) fail "$initiator: the compare did not end in a pin: $outcome"; return 1 ;;
  esac
  return 0
}
