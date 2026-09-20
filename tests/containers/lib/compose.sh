#!/bin/sh
# The docker and compose verbs every scenario repeats, plus the pairing dance
# all three of the router-free scenarios start with.
#
# A scenario sources `assert.sh` and then this, and afterwards talks about
# SERVICES rather than container names: `up node-a1 node-a2`, `nx node-a1 tcr
# peer ls --json`. The container names are derived from the project the runner
# chose and never written down a second time, because a `--keep` run is
# inspected by that project name and a second spelling of it is a second thing
# to correct.
#
# HERE must be set by the scenario before this is sourced, the same way
# `assert.sh` needs SCENARIO. Both refusals are up front rather than a later
# empty path, because an empty compose file argument brings up the wrong
# project rather than failing.
: "${SCENARIO:?a scenario must set SCENARIO before sourcing compose.sh}"
: "${HERE:?a scenario must set HERE to its own directory before sourcing compose.sh}"

DOCKER="${DOCKER:-docker}"
COMPOSE_PROJECT="${COMPOSE_PROJECT:-tcr-peers-$SCENARIO}"
CONTAINERS="$(cd "$HERE/.." && pwd)"
COMPOSE_FILE="${COMPOSE_FILE:-$CONTAINERS/compose.yml}"
# shellcheck disable=SC2034 # read by the scenario that sourced this, not here
LIB="$CONTAINERS/lib"
# Every node writes the same two paths, because the entrypoint does.
PEERS=/scratch/.config/tcr-peers.json
DRIVER=/usr/local/bin/node-driver.sh
# The listen port every node in the cast is configured with. A scenario that
# changes it says so; the assertions about ports compare against this.
# shellcheck disable=SC2034 # read by the scenario that sourced this, not here
LISTEN_PORT=7755

# Somewhere for a command's output to be read twice without running it twice.
# Suffixed per process, because several scenarios can share a machine and a
# fixed path in /tmp is how one run reports a verdict about another's file.
SCRATCH="${TMPDIR:-/tmp}/tcr-containers-$SCENARIO-$$"
mkdir -p "$SCRATCH"
cleanup_scratch() { rm -rf "$SCRATCH"; }
trap cleanup_scratch EXIT

dc() { "$DOCKER" compose -p "$COMPOSE_PROJECT" -f "$COMPOSE_FILE" "$@"; }

# The container name compose gives a service in this project.
cname() { echo "$COMPOSE_PROJECT-$1-1"; }

# Run a command in one service's container.
nx() {
  service="$1"
  shift
  "$DOCKER" exec "$(cname "$service")" "$@"
}

# The same, with this scenario's standard input attached: for the verbs that
# take a secret on stdin rather than in an argument vector every process on the
# machine can read.
nxi() {
  service="$1"
  shift
  "$DOCKER" exec -i "$(cname "$service")" "$@"
}

# The same, detached: for a command that outlives the exec that started it.
nxd() {
  service="$1"
  shift
  "$DOCKER" exec -d "$(cname "$service")" "$@"
}

# Bring services up and wait for each one's peer listener.
up() {
  dc up -d "$@" >/dev/null 2>&1 || {
    fail "compose up $* refused"
    return 1
  }
  for service in "$@"; do
    if ! wait_for_log "$service" "peer listener up" 90 >/dev/null; then
      fail "$service: no 'peer listener up' in its boot log within 90s"
      return 1
    fi
  done
  return 0
}

# wait_for_log <service> <regex> [seconds]
# Print the newest matching line from that node's boot log, or nothing.
wait_for_log() {
  service="$1"
  pattern="$2"
  seconds="${3:-60}"
  i=0
  while [ "$i" -lt "$seconds" ]; do
    line="$("$DOCKER" exec "$(cname "$service")" grep -hE "$pattern" /scratch/boot.log 2>/dev/null | tail -1)"
    if [ -n "$line" ]; then
      echo "$line"
      return 0
    fi
    sleep 1
    i=$((i + 1))
  done
  return 1
}

# This node's own peer id, in the SHORT form `tcr peer id` prints, `tcr-` plus
# the first ten characters of the wire form.
#
# There is no CLI that prints this node's own id in the wire form, and that is
# deliberate: the short form is one-way, and `PeerId::parse` refuses it so that
# a truncated id can never silently resolve to a pinned peer. A scenario that
# needs the wire form reads it off the OTHER node's listing, where the pinning
# handshake wrote it, which is what `pinned_wire_id` is for.
own_short_id() { nx "$1" tcr peer id --peers "$PEERS" | tr -d '\r\n'; }

# The one node id a listing pins, in the wire form every verb that names a peer
# takes. Empty when the listing pins nothing or more than one row: a scenario
# that took the first of several would assert about whichever the file happened
# to list first.
pinned_wire_id() {
  ids="$(python3 "$LIB/peer-read.py" nodes < "$1" 2>/dev/null || true)"
  if [ "$(echo "$ids" | grep -c .)" != "1" ]; then
    return 1
  fi
  echo "$ids" | tr -d '\r\n'
}

# A wire id as `PeerId::display` renders it, so a wire id read off one node's
# file can be held against the short form the other node prints for itself.
short_of() { printf 'tcr-%s\n' "$(printf '%s' "$1" | cut -c1-10)"; }

# `tcr peer ls --json` for one node, into a file, so the several assertions that
# read it read one listing rather than three taken seconds apart.
ls_json() {
  service="$1"
  out="$2"
  nx "$service" tcr peer ls --peers "$PEERS" --json > "$out" 2>/dev/null
}

# pair_nodes <initiator> <responder> <responder-addr>
#
# The whole six-digit pairing, driven from outside: knock, wait for the request
# to queue at the responder, accept it there, read the digits off BOTH screens,
# confirm they are the same, and answer. It prints its own assertion lines,
# because "the two Macs showed the same six digits" is a fact worth a line
# wherever it happens and not only in the scenario named after it.
#
# One direction only. A pairing writes a row on the side that ran `tcr peer
# pair` and, deliberately, none on the side that accepted: the listener's own
# comment says a first pairing "ENDS here, and it writes no pin". So a scenario
# that wants both files to pin the other calls this twice, pointed each way,
# which is exactly what the CLI's closing line tells an operator to do.
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
    sleep 1
    i=$((i + 1))
  done
  if [ -z "$queued" ]; then
    fail "$responder: no pending row for instance $instance within 60s"
    return 1
  fi
  pass "$responder queued one request naming instance $instance"

  accepted="$(nx "$responder" tcr peer accept "$instance" --peers "$PEERS" 2>&1)"
  case "$accepted" in
    *"instance=$instance"*) pass "$responder accepted instance $instance" ;;
    *)
      fail "$responder: accept did not name the instance it approved: $accepted"
      return 1
      ;;
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
    *)
      fail "$initiator: the compare did not end in a pin: $outcome"
      return 1
      ;;
  esac
  return 0
}
