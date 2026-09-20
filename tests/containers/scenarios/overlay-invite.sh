#!/bin/sh
# overlay-invite
# An invite over the overlay: two Macs that share a network no NAT sits in.
#
# Cast: node-a1-overlay and node-b1-overlay, each on its home network AND on
# `overlay`, plus router-a and router-b with no mapping asked of either. The
# overlay stands in for a mesh VPN's reachability and nothing else: no
# WireGuard, no key exchange, no MagicDNS, no ACLs. It models a tailnet, not
# a second LAN: its subnet sits inside 100.64.0.0/10 and each overlay node's
# entrypoint renames the interface holding that address to `utun0`, because
# `pair::is_tailnet` only ranks an address first when both of those are true.
#
# Steps
#   1. up both routers, then both overlay nodes
#   2. on node-a1-overlay: tcr peer invite
#   3. on node-b1-overlay: tcr peer join --stdin (the key, on stdin, never argv)
#
# Assertions
#   - the key's FIRST address is the overlay one (100.64.99.11:7755), ranked
#     ahead of the LAN address, because that is the one a friend elsewhere
#     can dial
#   - node-b1-overlay's join answers ok
#   - both sides pin the other: the join handshake pins on the spot, no
#     six-digit compare and no second command
#   - the joiner's row for node-a1 carries the overlay address
#
# `pair::dial_addresses` ranks a tailnet address ahead of a LAN one by
# `pair::is_tailnet`, not by which interface `if-addrs` happens to walk to
# first. This scenario measures whether that ranking holds for real; if the
# harness's overlay address does not come first, this is the scenario that
# says so.
set -eu

HERE="$(cd "$(dirname "$0")" && pwd)"
SCENARIO="overlay-invite"
export SCENARIO
# shellcheck source=../lib/assert.sh
. "$HERE/../lib/assert.sh"

A1_OVERLAY="100.64.99.11:7755"

if [ "${NETLAB:-0}" = "1" ]; then
  TOPOLOGY="$HERE/../topologies/two-homes-overlay.json"
  export TOPOLOGY
  NETLAB_UPSTREAM="http://5.5.5.20:8080"
  export NETLAB_UPSTREAM
  # shellcheck source=../lib/netlab.sh
  . "$HERE/../lib/netlab.sh"
  # The lab names the overlay interface utun0 the moment it creates the wire
  # (`is_overlay_addr` in netlab), so there is no rename to wait on: the router
  # wait below is the only startup ordering this scenario needs under netlab.
  up_with_routers router-a router-b -- node-a1-overlay node-b1-overlay || { finish; exit 1; }
  pass "both routers are up, no mapping asked of either"
else
  # shellcheck source=../lib/compose.sh
  . "$HERE/../lib/compose.sh"

  # The compose services this scenario needs, for the runner and for a reader.
  SERVICES="node-a1-overlay node-b1-overlay router-a router-b"
  export SERVICES

  # The routers first: home-a and home-b are `internal: true`, so each node's
  # route to anything off its own home network goes through its router, and
  # node-b1-overlay's attempt at node-a1's LAN address (which this scenario
  # expects to lose to the overlay one) has to fail on a router that exists,
  # not hang on ARP for one that does not.
  dc up -d router-a router-b >/dev/null 2>&1 || {
    fail "compose up router-a router-b refused"
    finish
    exit 1
  }
  # The router's own boot line goes to stdout, never to a file under /scratch:
  # `wait_for_log` (compose.sh) greps a boot log inside the container, which is
  # a node convention the router image does not share, so this scenario reads
  # its line off `docker logs` instead.
  wait_for_router_log() {
    service="$1"
    seconds="${2:-30}"
    i=0
    while [ "$i" -lt "$seconds" ]; do
      if "$DOCKER" logs "$(cname "$service")" 2>&1 | grep -q "router: up:"; then
        return 0
      fi
      sleep 1
      i=$((i + 1))
    done
    return 1
  }
  if ! wait_for_router_log router-a 30; then
    fail "router-a: no 'router: up:' on its stdout within 30s"
    finish
    exit 1
  fi
  if ! wait_for_router_log router-b 30; then
    fail "router-b: no 'router: up:' on its stdout within 30s"
    finish
    exit 1
  fi
  pass "both routers are up, no mapping asked of either"

  up node-a1-overlay node-b1-overlay || { finish; exit 1; }
fi

# --- mint ---------------------------------------------------------------
minted="$SCRATCH/minted.txt"
nx node-a1-overlay tcr peer invite --peers "$PEERS" --label node-b1-overlay > "$minted" 2>&1 || true

token="$(grep -m1 '^tcr-join:' "$minted" || true)"
if [ -z "$token" ]; then
  fail "node-a1-overlay minted no key: $(cat "$minted")"
  finish
  exit 1
fi
pass "node-a1-overlay minted an invite key"

# One line per address the key carries, `peer invite: <kind> <addr>`. Both
# home-a and overlay read as `lan`, so this greps every kind rather than one.
addr_lines="$(grep -E '^peer invite: (lan|chosen|tailscale|internet|from-key) ' "$minted" || true)"
echo "$addr_lines" | sed 's/^/overlay-invite: key carries: /'
first_addr="$(printf '%s\n' "$addr_lines" | head -1 | awk '{print $NF}')"
if [ "$first_addr" = "$A1_OVERLAY" ]; then
  pass "the key's first address is the overlay one, $first_addr, ranked ahead of the LAN address"
else
  fail "the key's first address is [$first_addr], not the overlay address $A1_OVERLAY"
fi

# --- join -----------------------------------------------------------------
joined="$(printf '%s\n' "$token" | nxi node-b1-overlay tcr peer join --stdin --peers "$PEERS" --label node-b1-overlay 2>&1 || true)"
echo "$joined" | sed 's/^/overlay-invite: join: /'
joined_addr="$(printf '%s\n' "$joined" | sed -n 's/.*peer join: ok addr=\([^ ]*\).*/\1/p')"
if [ -n "$joined_addr" ]; then
  pass "node-b1-overlay joined, answered at $joined_addr"
else
  fail "node-b1-overlay's join did not report ok: $joined"
  finish
  exit 1
fi

# --- both trusted, with no second command -----------------------------------
a1_ls="$SCRATCH/a1-pinned.json"
b1_ls="$SCRATCH/b1-pinned.json"
ls_json node-a1-overlay "$a1_ls" || true
ls_json node-b1-overlay "$b1_ls" || true
a1_id="$(pinned_wire_id "$b1_ls" || true)"
b1_id="$(pinned_wire_id "$a1_ls" || true)"
if [ -n "$a1_id" ]; then
  pass "node-b1-overlay pins node-a1 after the join, no six-digit compare needed"
else
  fail "node-b1-overlay pins no single Mac after the join: $(cat "$b1_ls")"
fi
if [ -n "$b1_id" ]; then
  pass "node-a1-overlay pinned node-b1 back on the spot, with no second command"
else
  fail "node-a1-overlay pins no single Mac after the join: $(cat "$a1_ls")"
fi

# --- the joiner's row for node-a1 holds the overlay address ------------------
if [ -n "$a1_id" ]; then
  endpoints="$SCRATCH/b1-a1-endpoints.txt"
  if python3 "$LIB/peer-read.py" endpoints "$a1_id" < "$b1_ls" > "$endpoints" 2>&1; then
    if grep -q "^direct $A1_OVERLAY\$" "$endpoints"; then
      pass "node-b1-overlay's row for node-a1 holds the overlay address, $A1_OVERLAY"
    else
      fail "node-b1-overlay's row for node-a1 does not hold the overlay address: $(cat "$endpoints")"
    fi
  else
    fail "node-b1-overlay: could not read its own row for node-a1: $(cat "$endpoints")"
  fi
else
  fail "no node-a1 id pinned, so the row it would have written was never checked"
fi

finish
