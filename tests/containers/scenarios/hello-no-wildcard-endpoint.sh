#!/bin/sh
# hello-no-wildcard-endpoint
# No row anywhere records 0.0.0.0, and no row records an ephemeral source port.
#
# Cast: node-a1 (10.77.1.11) and node-a2 (10.77.1.12) on home-a, paired both
# ways. Both listen on 0.0.0.0:7755, which is the case that produced the
# defect: a node whose configured listen socket is the unspecified address has
# a wildcard to hand out, and a hello is where it would hand it out.
#
# Steps
#   1. up node-a1 node-a2, pair each way
#   2. on node-a2: tcr peer hello <node-a1 id>
#   3. on node-a1: tcr peer hello <node-a2 id>
#   4. on both: tcr peer ls --json, and read every endpoint on every row
#
# Assertions, one line per endpoint on either node
#   - its host is not 0.0.0.0 and not ::
#   - its port is the peer's listen port, not the source port of the connection
#     that carried the hello
#
# Both directions of hello, not one. The greeting writes on both sides, by two
# different code paths: the dialling CLI records what came back, and the
# answering server records what arrived, so a wildcard admitted by one of them
# and refused by the other would look clean from whichever side was asked.
set -eu

HERE="$(cd "$(dirname "$0")" && pwd)"
SCENARIO="hello-no-wildcard-endpoint"
export SCENARIO
# shellcheck source=../lib/assert.sh
. "$HERE/../lib/assert.sh"
# shellcheck source=../lib/compose.sh
. "$HERE/../lib/compose.sh"

# The compose services this scenario needs, for the runner and for a reader.
SERVICES="node-a1 node-a2"
export SERVICES

# shellcheck disable=SC2086 # SERVICES is a deliberate list of service names
up $SERVICES || { finish; exit 1; }

pair_nodes node-a2 node-a1 "10.77.1.11:$LISTEN_PORT" || { finish; exit 1; }
pair_nodes node-a1 node-a2 "10.77.1.12:$LISTEN_PORT" || { finish; exit 1; }

# Each node's wire id, read off the file of the Mac that pinned it. That is the
# only place it exists in the form `tcr peer hello` takes: `tcr peer id` prints
# the short display form, which `PeerId::parse` refuses on purpose.
a1_ls="$SCRATCH/a1-pinned.json"
a2_ls="$SCRATCH/a2-pinned.json"
ls_json node-a1 "$a1_ls" || true
ls_json node-a2 "$a2_ls" || true
a2_id="$(pinned_wire_id "$a1_ls" || true)"
a1_id="$(pinned_wire_id "$a2_ls" || true)"
if [ -z "$a1_id" ] || [ -z "$a2_id" ]; then
  fail "a node does not pin exactly one Mac after two pairings (a1=$a1_id a2=$a2_id)"
  finish
  exit 1
fi

# --- the greetings -------------------------------------------------------
say_hello() {
  # $1 the node greeting, $2 the id it greets.
  said="$(nx "$1" tcr peer hello "$2" --peers "$PEERS" 2>&1 || true)"
  case "$said" in
    *"peer hello: ok"*) pass "$1 greeted $2 and got an answer" ;;
    *) fail "$1 greeting $2: $said" ;;
  esac
}

say_hello node-a2 "$a1_id"
say_hello node-a1 "$a2_id"

# --- every endpoint on every row -----------------------------------------
# The rule, stated once: a recorded endpoint is dialable and answers on the
# peer's listen port. A wildcard host is not dialable, and the ephemeral port a
# hello arrived from belongs to that one connection and to nothing else.
check_endpoints() {
  # $1 the node whose file this is, $2 the peer id whose row to read.
  who="$1"
  wanted="$2"
  listing="$SCRATCH/$who-ls.json"
  endpoints="$SCRATCH/$who-endpoints.txt"
  if ! ls_json "$who" "$listing"; then
    fail "$who: could not read its own peer listing"
    return
  fi
  if ! python3 "$LIB/peer-read.py" endpoints "$wanted" < "$listing" > "$endpoints" 2>&1; then
    fail "$who: no pinned row for $wanted, so its endpoints prove nothing: $(cat "$endpoints")"
    return
  fi
  if [ ! -s "$endpoints" ]; then
    fail "$who: the row for $wanted holds no endpoint at all, so an absence of wildcards proves nothing"
    return
  fi
  while read -r kind value; do
    case "$kind" in
      direct) ;;
      *)
        pass "$who holds a $kind path to $value, which carries no socket to be wrong about"
        continue
        ;;
    esac
    host="${value%:*}"
    port="${value##*:}"
    if [ "$host" = "0.0.0.0" ] || [ "$host" = "[::]" ] || [ "$host" = "::" ]; then
      fail "$who records $value for $wanted: $host is the unspecified address, which nothing can dial"
    elif [ "$port" != "$LISTEN_PORT" ]; then
      fail "$who records $value for $wanted: port $port is not the peer's listen port $LISTEN_PORT"
    else
      pass "$who records $value for $wanted: a dialable host on the listen port"
    fi
  done < "$endpoints"
}

check_endpoints node-a1 "$a2_id"
check_endpoints node-a2 "$a1_id"

finish
