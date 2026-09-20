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

ROUTER_A_OUTSIDE=5.5.5.2

if [ "${NETLAB:-0}" = "1" ]; then
  TOPOLOGY="$HERE/../topologies/two-homes-two-routers-upnp.json"
  export TOPOLOGY
  NETLAB_UPSTREAM="http://5.5.5.20:8080"
  export NETLAB_UPSTREAM
  # shellcheck source=../lib/netlab.sh
  . "$HERE/../lib/netlab.sh"
else
  # shellcheck source=../lib/compose.sh
  . "$HERE/../lib/compose.sh"

  # The compose services this scenario needs, for the runner and for a reader.
  SERVICES="node-a1 node-b1 router-a router-b"
  export SERVICES

  ROUTER_A_UPNP=on
  export ROUTER_A_UPNP
fi

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
# `iptables-legacy`, not the plain `iptables` in this image: miniupnpd links
# against libip4tc, the legacy library that manages the `ip_tables` kernel
# module directly, while `iptables` here is the nft-compat build (libnftnl,
# managing nftables over netlink instead) and cannot see what miniupnpd wrote,
# nor miniupnpd what it writes. Reading the chain miniupnpd actually used is
# the evidence "a lease was recorded" without a lease-listing client this
# image does not carry.
nat_rules="$(nx router-a sh -c 'iptables-legacy -t nat -L MINIUPNPD -n 2>&1' || true)"
echo "$nat_rules" | sed 's/^/nat-upnp-mapping: router-a nat chain: /'
case "$nat_rules" in
  *"dpt:$LISTEN_PORT"*)
    pass "router-a's MINIUPNPD nat chain carries a rule for port $LISTEN_PORT: $(echo "$nat_rules" | grep -m1 "dpt:$LISTEN_PORT")" ;;
  *)
    fail "router-a's MINIUPNPD nat chain carries no rule for port $LISTEN_PORT: $nat_rules" ;;
esac

# --- wait for node-a1's own serving process to hold the mapping --------------
# `reach --map` above made and released a one-off probe mapping; it is not
# node-a1's own long-lived server holding one, and `peer invite` reads the
# server's held mapping to decide whether the key carries an internet
# address. The keeper inside that server wakes every five seconds, so this
# gives it several cycles rather than racing the first one.
held_line=""
i=0
while [ "$i" -lt 30 ]; do
  probe="$(nx node-a1 tcr peer reach --peers "$PEERS" 2>&1 || true)"
  case "$probe" in
    *"reach: held-mapping: none"*) ;;
    *"reach: held-mapping:"*) held_line="$(echo "$probe" | grep -m1 'reach: held-mapping:')" ;;
  esac
  [ -n "$held_line" ] && break
  sleep 1
  i=$((i + 1))
done
if [ -n "$held_line" ]; then
  pass "node-a1's serving process holds the mapping: $held_line"
else
  fail "node-a1's serving process never recorded a held mapping within 30s"
fi

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

# Not a harness bug: `tcr peer invite` here is a fresh `docker exec` process,
# not the long-lived `tcr --headless` server holding the mapping, and
# `reach::external_socket()` (src/peer/reach.rs) is deliberately a
# process-local register, "the keeper runs on its own thread for the life of
# the listener" (its own doc comment). `tcr peer reach` sees the mapping
# because it reads the PERSISTED state file the server writes
# (`peer::state::load`, the "held-mapping" fact above); `mint_invite`
# (src/peer/pair.rs) calls the in-memory register instead, so a key minted
# out-of-process against a live server's peers file can never carry an
# internet address, whatever the router did. That is the real gap this
# assertion is expected to find, and it needs an edit under src/, not here.
case "$(cat "$minted")" in
  *"peer invite: internet $ROUTER_A_OUTSIDE:$LISTEN_PORT"*)
    pass "the key's first address is router-a's mapped outside socket, $ROUTER_A_OUTSIDE:$LISTEN_PORT" ;;
  *) fail "the key does not carry router-a's mapped outside socket: $(cat "$minted")" ;;
esac

# --- join, from the other home -----------------------------------------------
# Downstream of the assertion above: a key with no internet address has
# nothing but the LAN one to try, so this fails whenever that one does,
# whatever the join itself proves. It stays last so every network-independent
# fact above it (the mapping exists, reach --map named it) is proven either
# way, and its own FAIL line says which of the two things it saw: no route to
# the LAN address (this host's home-a and home-b stay apart, the ordinary
# case) or a route that reached it anyway (OrbStack routing directly between
# the two `internal: true` home networks, `orb config show` ->
# `machine.docker.isolated: false`), which would mean the mapping was never
# exercised at all.
joined="$(printf '%s\n' "$key" | nxi node-b1 tcr peer join --stdin --peers "$PEERS" 2>&1 || true)"
echo "$joined" | sed 's/^/nat-upnp-mapping: join: /'
joined_mapped=""
case "$joined" in
  *"peer join: ok addr=$ROUTER_A_OUTSIDE:$LISTEN_PORT"*) joined_mapped=yes ;;
esac

joined_via_mapping=""
if [ -n "$joined_mapped" ]; then
  after_ls="$SCRATCH/b1-pinned.json"
  after_endpoints="$SCRATCH/b1-endpoints.txt"
  if ls_json node-b1 "$after_ls" && python3 "$LIB/peer-read.py" nodes < "$after_ls" >/dev/null 2>&1; then
    a1_id="$(pinned_wire_id "$after_ls" || true)"
    if [ -n "$a1_id" ] && python3 "$LIB/peer-read.py" endpoints "$a1_id" < "$after_ls" > "$after_endpoints" 2>&1 \
      && grep -q "^direct $ROUTER_A_OUTSIDE:$LISTEN_PORT$" "$after_endpoints"; then
      joined_via_mapping=yes
    fi
  fi
fi

case "$joined" in
  *"Network unreachable"*)
    reason="downstream of the key-carries-no-internet-address gap above: node-b1 had \
only node-a1's LAN address to try, and this host keeps home-a and home-b apart, \
the mapping was never exercised" ;;
  *"peer join: ok"*)
    reason="the key's LAN address answered directly; likely cause: this host's OrbStack \
routes between home-a and home-b regardless of internal:true (machine.docker.isolated: \
false in \`orb config show\`), so the mapping was never exercised either way" ;;
  *)
    reason="see the join output above" ;;
esac

if [ -n "$joined_via_mapping" ]; then
  pass "node-b1 joined through router-a's mapped port, $ROUTER_A_OUTSIDE:$LISTEN_PORT, and its pinned row for node-a1 carries it"
else
  fail "node-b1 did not join through router-a's mapped port ($joined); $reason"
fi

finish
