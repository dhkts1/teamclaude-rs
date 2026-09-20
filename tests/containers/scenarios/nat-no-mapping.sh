#!/bin/sh
# nat-no-mapping
# An invite key across two NATs, with a router that refuses NAT-PMP.
#
# Cast: node-a1 behind router-a (UPNP=off), node-b1 behind router-b, upstream.
#
# Steps
#   1. up router-a router-b node-a1 node-b1 with ROUTER_A_UPNP=off
#   2. on node-a1: tcr peer internet on, then tcr peer reach --map
#   3. on node-a1: tcr peer invite, capture the key
#   4. on node-b1: tcr peer join <key>
#
# Assertions
#   - node-a1's `tcr peer reach --map` prints the no-mapping line, naming the
#     refusal rather than a silence (the gateway answers 5351 with ICMP
#     unreachable, which is exactly the owner's router)
#   - the log carries `peer internet: the router would not map this node's
#     listener port`
#   - the invite key node-a1 mints carries no internet address, only its LAN
#     one, because nothing was ever mapped
#   - node-b1's join FAILS, and says the address it could not reach
#
# Today: expected green in the sense that the failure is the correct one. The
# value is the second half: that the refusal is legible and does not retry
# every five seconds forever.
set -eu

HERE="$(cd "$(dirname "$0")" && pwd)"
SCENARIO="nat-no-mapping"
export SCENARIO
# shellcheck source=../lib/assert.sh
. "$HERE/../lib/assert.sh"
# shellcheck source=../lib/compose.sh
. "$HERE/../lib/compose.sh"

# The compose services this scenario needs, for the runner and for a reader.
SERVICES="node-a1 node-b1 router-a router-b"
export SERVICES

A1_HOME=10.77.1.11

ROUTER_A_UPNP=off
export ROUTER_A_UPNP

up_with_routers router-a router-b -- node-a1 node-b1 || { finish; exit 1; }
pass "router-a router-b node-a1 node-b1 all reported up"

# --- internet on -----------------------------------------------------------
internet_out="$(nx node-a1 tcr peer internet on --peers "$PEERS" 2>&1 || true)"
case "$internet_out" in
  *"peer.internet: on"*) pass "node-a1: peer internet on: $internet_out" ;;
  *) fail "node-a1: peer internet on did not say it turned on: $internet_out" ;;
esac

# The background keeper this switch starts wakes every five seconds
# (`INTERNET_POLL_INTERVAL`), asks router-a for a mapping, and logs its own
# refusal at warn once. Thirty seconds is six wakes, generous over that.
refusal_line="$(wait_for_log node-a1 "the router would not map this node.s listener port" 30 || true)"
if [ -n "$refusal_line" ]; then
  pass "node-a1's log carries the refusal: $refusal_line"
else
  fail "node-a1's log never carried 'peer internet: the router would not map this node's listener port' within 30s"
fi

# --- reach --map -------------------------------------------------------------
reach_out="$(nx node-a1 tcr peer reach --map --peers "$PEERS" 2>&1 || true)"
echo "$reach_out" | sed 's/^/nat-no-mapping: reach: /'
case "$reach_out" in
  *"reach: no-mapping: this router will not open a port over either protocol"*)
    pass "node-a1's reach --map names the refusal rather than a silence" ;;
  *) fail "node-a1's reach --map did not print the no-mapping line: $reach_out" ;;
esac

# --- invite ------------------------------------------------------------------
minted="$SCRATCH/minted.txt"
nx node-a1 tcr peer invite --peers "$PEERS" --label joining-mac > "$minted" 2>&1 || true
key="$(grep -m1 '^tcr-join:' "$minted" || true)"
if [ -z "$key" ]; then
  fail "node-a1 minted no invite key: $(cat "$minted")"
  finish
  exit 1
fi
pass "node-a1 minted an invite key"

if grep -q "^peer invite: internet " "$minted"; then
  fail "the key carries an internet address even though nothing was mapped: $(cat "$minted")"
else
  pass "the key carries no internet address, since nothing was ever mapped"
fi
case "$(cat "$minted")" in
  *"peer invite: lan $A1_HOME:$LISTEN_PORT"*)
    pass "the key carries node-a1's LAN address, $A1_HOME:$LISTEN_PORT" ;;
  *) fail "the key does not carry node-a1's LAN address: $(cat "$minted")" ;;
esac

# --- join, from the other home ----------------------------------------------
joined="$(printf '%s\n' "$key" | nxi node-b1 tcr peer join --stdin --peers "$PEERS" 2>&1 || true)"
echo "$joined" | sed 's/^/nat-no-mapping: join: /'
case "$joined" in
  *"peer join: nothing answered at any address this key carries: $A1_HOME:$LISTEN_PORT"*)
    pass "node-b1's join failed and named the address it could not reach, $A1_HOME:$LISTEN_PORT" ;;
  *"peer join: ok"*)
    fail "node-b1's join succeeded, which nothing on this router should allow: $joined" ;;
  *) fail "node-b1's join failed for a reason other than the one this router should give: $joined" ;;
esac

finish
