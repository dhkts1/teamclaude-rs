#!/bin/sh
# nat-upnp-mapping
# The same two NATs, with UPnP available on the inviter's router.
#
# Cast: as nat-no-mapping, but ROUTER_A_UPNP=on.
#
# Steps
#   1. up with ROUTER_A_UPNP=on
#   2. on node-a1: tcr peer internet on, then tcr peer reach --map
#   3. on node-a1: tcr peer invite, capture the key
#   4. on node-b1: tcr peer join <key>
#   5. on router-a: the iptables rules miniupnpd installed for the mapping
#
# Assertions
#   - `tcr peer reach --map` prints an external address on router-a's outside
#     address (over upnp, since nothing here speaks nat-pmp)
#   - router-a's own MINIUPNPD nat chain carries a rule for the mapped port
#   - node-b1's join succeeds, and its pinned row for node-a1 carries the
#     mapped external address rather than the LAN one
#
# Today: the fallback from a refused NAT-PMP to UPnP is in this tree
# (`MappingKeeper::map`, the `no_natpmp_service` arm). Whether miniupnpd's own
# refusals are legible from the node side is the open question this answers.
set -eu

HERE="$(cd "$(dirname "$0")" && pwd)"
SCENARIO="nat-upnp-mapping"
export SCENARIO
# shellcheck source=../lib/assert.sh
. "$HERE/../lib/assert.sh"
# shellcheck source=../lib/compose.sh
. "$HERE/../lib/compose.sh"

# The compose services this scenario needs, for the runner and for a reader.
SERVICES="node-a1 node-b1 router-a router-b"
export SERVICES

ROUTER_A_OUTSIDE=198.51.100.2

ROUTER_A_UPNP=on
export ROUTER_A_UPNP

up_with_routers router-a router-b -- node-a1 node-b1 || { finish; exit 1; }
pass "router-a router-b node-a1 node-b1 all reported up"

upnp_line="$(wait_router_log router-a '^router: upnp:' 15 || true)"
if [ -n "$upnp_line" ]; then
  pass "router-a's log says miniupnpd is serving: $upnp_line"
else
  fail "router-a's log never carried 'router: upnp:' within 15s"
fi

# --- internet on -----------------------------------------------------------
internet_out="$(nx node-a1 tcr peer internet on --peers "$PEERS" 2>&1 || true)"
case "$internet_out" in
  *"peer.internet: on"*) pass "node-a1: peer internet on: $internet_out" ;;
  *) fail "node-a1: peer internet on did not say it turned on: $internet_out" ;;
esac

# --- reach --map -------------------------------------------------------------
# The keeper's own std::thread also wakes every five seconds and may already
# hold the mapping by the time this runs; either way `reach --map` asks the
# router itself, so it reports what is true right now rather than a cached
# state.
reach_out="$(nx node-a1 tcr peer reach --map --peers "$PEERS" 2>&1 || true)"
echo "$reach_out" | sed 's/^/nat-upnp-mapping: reach: /'
case "$reach_out" in
  *"reach: mapping: tcp "*"over upnp"*)
    pass "node-a1's reach --map reports a mapping made over upnp: $(echo "$reach_out" | grep -m1 'reach: mapping:')" ;;
  *)
    fail "node-a1's reach --map did not report a upnp mapping: $reach_out" ;;
esac
case "$reach_out" in
  *"reach: external-address: $ROUTER_A_OUTSIDE"*)
    pass "node-a1's reach --map names router-a's own outside address: $(echo "$reach_out" | grep -m1 'reach: external-address:')" ;;
  *)
    fail "node-a1's reach --map did not name router-a's outside address ($ROUTER_A_OUTSIDE): $reach_out" ;;
esac

# --- what miniupnpd actually installed on router-a --------------------------
# The iptables backend miniupnpd builds against manages its own chain
# (MINIUPNPD) for whatever it grants; reading it is the evidence "a lease was
# recorded" without a lease-listing client this image does not carry.
nat_rules="$(nx router-a sh -c 'iptables -t nat -L MINIUPNPD -n 2>&1' || true)"
echo "$nat_rules" | sed 's/^/nat-upnp-mapping: router-a nat chain: /'
case "$nat_rules" in
  *"dpt:$LISTEN_PORT"*)
    pass "router-a's MINIUPNPD nat chain carries a rule for port $LISTEN_PORT: $(echo "$nat_rules" | grep -m1 "dpt:$LISTEN_PORT")" ;;
  *)
    fail "router-a's MINIUPNPD nat chain carries no rule for port $LISTEN_PORT: $nat_rules" ;;
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

case "$(cat "$minted")" in
  *"peer invite: internet $ROUTER_A_OUTSIDE:$LISTEN_PORT"*)
    pass "the key's first address is router-a's mapped outside socket, $ROUTER_A_OUTSIDE:$LISTEN_PORT" ;;
  *) fail "the key does not carry router-a's mapped outside socket: $(cat "$minted")" ;;
esac

# --- join, from the other home ----------------------------------------------
joined="$(printf '%s\n' "$key" | nxi node-b1 tcr peer join --stdin --peers "$PEERS" 2>&1 || true)"
echo "$joined" | sed 's/^/nat-upnp-mapping: join: /'
case "$joined" in
  *"peer join: ok addr=$ROUTER_A_OUTSIDE:$LISTEN_PORT"*)
    pass "node-b1 joined through router-a's mapped port, $ROUTER_A_OUTSIDE:$LISTEN_PORT" ;;
  *) fail "node-b1's join did not succeed through the mapped port: $joined" ;;
esac

after_ls="$SCRATCH/b1-pinned.json"
after_endpoints="$SCRATCH/b1-endpoints.txt"
if ls_json node-b1 "$after_ls" && python3 "$LIB/peer-read.py" nodes < "$after_ls" >/dev/null 2>&1; then
  a1_id="$(pinned_wire_id "$after_ls" || true)"
  if [ -n "$a1_id" ] && python3 "$LIB/peer-read.py" endpoints "$a1_id" < "$after_ls" > "$after_endpoints" 2>&1; then
    if grep -q "^direct $ROUTER_A_OUTSIDE:$LISTEN_PORT$" "$after_endpoints"; then
      pass "node-b1's pinned row for node-a1 carries the mapped address, $ROUTER_A_OUTSIDE:$LISTEN_PORT"
    else
      fail "node-b1's pinned row for node-a1 does not carry the mapped address: $(cat "$after_endpoints")"
    fi
  else
    fail "node-b1: could not read its own row for node-a1 after the join"
  fi
else
  fail "node-b1: could not read its own peer listing after the join"
fi

finish
