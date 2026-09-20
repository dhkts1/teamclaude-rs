#!/bin/sh
# knock-nondefault-port
# A Mac listening somewhere other than 7755 is paired back on that port.
#
# Cast: node-a1 on the default 7755, node-a2 with PEER_LISTEN=0.0.0.0:7766.
# node-a2's non-default port is not something the shared compose.yml
# (phase 3's file) can be asked for from here, so it comes from
# knock-nondefault-port.override.yml, a second compose file this scenario
# merges in with an extra `-f`.
#
# Steps
#   1. up node-a1, node-a2 with the override
#   2. on node-a2: tcr peer pair 10.77.1.11:7755 (node-a2 knocks node-a1)
#   3. read node-a1's pending row for that knock BEFORE accepting it
#   4. accept on node-a1, compare the six digits, node-a2 pins node-a1
#   5. node-a1 pairs BACK, using exactly the address its own pending row
#      printed, so node-a1 pins node-a2 too
#
# Assertions
#   - node-a1's pending row prints 10.77.1.12:7766, node-a2's real listen
#     port from the 1.1.9 listen_port field, never a source port and never
#     the default 7755
#   - node-a2 pins node-a1 after the first compare
#   - node-a1 pins node-a2 back, dialling the exact address its pending row
#     named
set -eu

HERE="$(cd "$(dirname "$0")" && pwd)"
SCENARIO="knock-nondefault-port"
export SCENARIO
# shellcheck source=../lib/assert.sh
. "$HERE/../lib/assert.sh"
# shellcheck source=../lib/compose.sh
. "$HERE/../lib/compose.sh"

# The compose services this scenario needs, for the runner and for a reader.
SERVICES="node-a1 node-a2"
export SERVICES

OVERRIDE="$HERE/knock-nondefault-port.override.yml"
A1_ADDR="10.77.1.11:$LISTEN_PORT"
A2_HOST=10.77.1.12
A2_PORT=7766
A2_DIAL="$A2_HOST:$A2_PORT"

# up(), from compose.sh, with one extra compose file layered on: node-a2's
# non-default listen port. Everything after `up -d` is copied from up() so
# the boot wait is the same one every other scenario gets.
dc -f "$OVERRIDE" up -d node-a1 node-a2 >/dev/null 2>&1 || {
  fail "compose up node-a1 node-a2 (with the port override) refused"
  finish
  exit 1
}
for service in node-a1 node-a2; do
  if ! wait_for_log "$service" "peer listener up" 90 >/dev/null; then
    fail "$service: no 'peer listener up' in its boot log within 90s"
    finish
    exit 1
  fi
done

# --- node-a2 knocks node-a1 --------------------------------------------------
if ! nxd node-a2 "$DRIVER" pair-start "$A1_ADDR"; then
  fail "node-a2: could not start a pairing with $A1_ADDR"
  finish
  exit 1
fi
instance="$(nx node-a2 "$DRIVER" pair-instance | tr -d '\r\n')"
if [ -z "$instance" ]; then
  fail "node-a2: the knock at $A1_ADDR announced no instance id"
  finish
  exit 1
fi
pass "node-a2 knocked at $A1_ADDR as instance $instance"

# --- read node-a1's pending row before it is accepted -----------------------
pending_json=""
i=0
while [ "$i" -lt 60 ]; do
  candidate="$(nx node-a1 tcr peer pending --peers "$PEERS" --json 2>/dev/null || true)"
  if printf '%s' "$candidate" | grep -q "$instance"; then
    pending_json="$candidate"
    break
  fi
  sleep 1
  i=$((i + 1))
done
if [ -z "$pending_json" ]; then
  fail "node-a1: no pending row for instance $instance within 60s"
  finish
  exit 1
fi
pass "node-a1 queued one request naming instance $instance"

# peer-read.py reads a peer listing's pinned rows, not a pending row, so the
# pending fields are pulled here with the same tool the driver's own
# `event_field` uses: a `sed` capture, never a second JSON parser to keep in
# step with the wire shape. One pending row is the only thing this scenario
# ever queues, so the first match of each field in the JSON is the one this
# knock wrote.
#
# `pending_row_for_readers` (src/main.rs) already rewrites `addr` to
# `Knock::dial_address()` for every reader, `tcr peer pending --json`
# included: the host:port form is the "addr" field itself, not addr plus a
# separately-read listenPort tacked on again. `listenPort` is read too, only
# to say which of the two facts (the combined address, or the raw port) a
# scenario that finds this red should paste.
dial="$(printf '%s' "$pending_json" | sed -n 's/.*"addr":"\([^"]*\)".*/\1/p' | head -1)"
dial_port="$(printf '%s' "$pending_json" | sed -n 's/.*"listenPort":\([0-9]*\).*/\1/p' | head -1)"
echo "knock-nondefault-port: node-a1's pending row: addr=$dial listenPort=$dial_port"
if [ "$dial" = "$A2_DIAL" ]; then
  pass "node-a1's pending row prints $dial, node-a2's real listen port, not a source port and not $LISTEN_PORT"
else
  fail "node-a1's pending row printed [$dial], wanted $A2_DIAL"
fi

# --- accept, compare, node-a2 pins node-a1 ----------------------------------
accepted="$(nx node-a1 tcr peer accept "$instance" --peers "$PEERS" 2>&1)"
case "$accepted" in
  *"instance=$instance"*) pass "node-a1 accepted instance $instance" ;;
  *)
    fail "node-a1: accept did not name the instance it approved: $accepted"
    finish
    exit 1
    ;;
esac

initiator_code="$(nx node-a2 "$DRIVER" pair-code | tr -d '\r\n')"
responder_line="$(wait_for_log node-a1 "peer pairing: compare these six digits" 60)"
responder_code="$(echo "$responder_line" | sed -n 's/.*code=\([0-9][0-9]*\).*/\1/p')"
if [ -z "$initiator_code" ] || [ -z "$responder_code" ]; then
  fail "node-a2/node-a1: a six-digit compare with only one screen ($initiator_code/$responder_code)"
  finish
  exit 1
fi
if [ "$initiator_code" = "$responder_code" ]; then
  pass "node-a2 and node-a1 show the same six digits ($initiator_code)"
else
  fail "node-a2 shows $initiator_code and node-a1 shows $responder_code"
  finish
  exit 1
fi

nx node-a2 "$DRIVER" pair-answer "$initiator_code" >/dev/null
outcome="$(nx node-a2 "$DRIVER" pair-wait 2>&1)"
case "$outcome" in
  *'"event":"trusted"'*) pass "node-a2 pinned node-a1 after the compare" ;;
  *)
    fail "node-a2: the compare did not end in a pin: $outcome"
    finish
    exit 1
    ;;
esac

# --- node-a1 pairs back, at exactly the address its own pending row printed -
if [ "$dial" != "$A2_DIAL" ]; then
  fail "not pairing back at [$dial]: it never matched node-a2's real listen address, so the closing step would prove nothing"
  finish
  exit 1
fi
pair_nodes node-a1 node-a2 "$dial" || { finish; exit 1; }

finish
