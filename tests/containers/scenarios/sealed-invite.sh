#!/bin/sh
# sealed-invite
# Mode B end to end: an ask that names no address, a reply that seals a real
# join key to it, and the join that key runs the moment the reply opens.
#
# Cast: node-a1-overlay (10.77.1.11 on home-a, 100.64.99.11 on overlay) and
# node-b1-overlay (10.77.2.11 on home-b, 100.64.99.12 on overlay), plus
# router-a and router-b with no mapping asked of either, `overlay-invite`'s
# own cast: two Macs on separate home networks, never paired before this
# scenario runs, reachable only over the tailnet-shaped overlay.
#
# This is the cast the design named. The FIRST version of this scenario used
# node-a1/node-b1's home addresses because the reply then carried a bare
# address and the dial that followed was a knock, and
# `listener::internet_admission` refuses a knock or a first pairing from
# outside LAN scope by design (`src/peer/listener.rs`, `is_lan_scope_v4`
# deliberately excludes `100.64.0.0/10`). The reply now carries a join key
# instead, and off-LAN admission answers `Enrol` unconditionally, so the
# overlay cast the design asked for now completes.
#
# Steps
#   1. up both routers, then both overlay nodes
#   2. on node-a1-overlay: tcr peer invite --sealed, the ask
#   3. on node-b1-overlay: tcr peer join --stdin fed the ask, the reply
#   4. on node-a1-overlay: tcr peer invite --reply --stdin fed the reply:
#      opens it and joins on the spot
#   5. on node-a1-overlay: tcr peer invite --plain, a v2 key, minted in the
#      same run, the positive control
#
# Assertions
#   - the ask names neither node's address
#   - the reply names neither node's address
#   - the same grep over the --plain v2 key finds node-b1-overlay's address:
#     the positive control that proves the grep can fire at all
#   - opening the reply joins immediately and names the address it joined at
#   - both sides pin the other, with no second command and no six-digit
#     compare
#
# The address regex is deliberately loose (a dotted quad OR a bracketed run of
# hex and colons) rather than matching this scenario's own two addresses by
# name: a check that only knew to look for 10.77.1.11 and 10.77.2.11 would
# pass on a build that leaked a THIRD address into the blob.
set -eu

HERE="$(cd "$(dirname "$0")" && pwd)"
SCENARIO="sealed-invite"
export SCENARIO
# shellcheck source=../lib/assert.sh
. "$HERE/../lib/assert.sh"

B1_OVERLAY="100.64.99.12:7755"

# A dotted-quad IPv4 address, or a bracketed run of hex and colons (an IPv6
# one): what a person reading a chat window would recognise as "an address",
# the same shape the design's own contract names.
ADDR_RE='[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+|\[[0-9a-fA-F:]+\]'

if [ "${NETLAB:-0}" = "1" ]; then
  TOPOLOGY="$HERE/../topologies/two-homes-overlay.json"
  export TOPOLOGY
  NETLAB_UPSTREAM="http://5.5.5.20:8080"
  export NETLAB_UPSTREAM
  # shellcheck source=../lib/netlab.sh
  . "$HERE/../lib/netlab.sh"
  up_with_routers router-a router-b -- node-a1-overlay node-b1-overlay || { finish; exit 1; }
  pass "both routers are up, no mapping asked of either"
else
  # shellcheck source=../lib/compose.sh
  . "$HERE/../lib/compose.sh"

  SERVICES="node-a1-overlay node-b1-overlay router-a router-b"
  export SERVICES

  # The routers first, `overlay-invite`'s own order: home-a and home-b are
  # `internal: true`, so each node's default route goes through its router, and
  # `reach.rs` reads that route to find the gateway it probes. Neither router is
  # asked for a mapping here; the overlay is what carries this scenario's join.
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
  dc up -d router-a router-b >/dev/null 2>&1 || {
    fail "compose up router-a router-b refused"
    finish
    exit 1
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

# --- mint an ask -----------------------------------------------------------
asked="$SCRATCH/asked.txt"
nx node-a1-overlay tcr peer invite --sealed --peers "$PEERS" > "$asked" 2>&1 || true
ask="$(grep -m1 '^tcr-invite:v1:' "$asked" || true)"
if [ -z "$ask" ]; then
  fail "node-a1-overlay minted no ask: $(cat "$asked")"
  finish
  exit 1
fi
pass "node-a1-overlay minted an ask: $(echo "$ask" | cut -c1-24)..."

if echo "$ask" | grep -qE "$ADDR_RE"; then
  fail "the ask names something that reads as an address: $ask"
else
  pass "the ask names no address (checked with a live grep, not by construction)"
fi

# --- answer it ---------------------------------------------------------------
answered="$SCRATCH/answered.txt"
printf '%s\n' "$ask" | nxi node-b1-overlay tcr peer join --stdin --peers "$PEERS" > "$answered" 2>&1 || true
reply="$(grep -m1 '^tcr-reply:v1:' "$answered" || true)"
if [ -z "$reply" ]; then
  fail "node-b1-overlay answered with no reply: $(cat "$answered")"
  finish
  exit 1
fi
pass "node-b1-overlay answered with a reply: $(echo "$reply" | cut -c1-24)..."

if echo "$reply" | grep -qE "$ADDR_RE"; then
  fail "the reply names something that reads as an address: $reply"
else
  pass "the reply names no address either (checked with a live grep)"
fi

# --- the positive control: the same grep over a --plain key finds one ------
# Minted in this same run, on this same Mac, so the control is over the exact
# grep and the exact regex the two checks above just passed under, not a
# hand-picked example from somewhere else.
plain="$SCRATCH/plain.txt"
nx node-a1-overlay tcr peer invite --plain --peers "$PEERS" > "$plain" 2>&1 || true
plain_key="$(grep -m1 '^tcr-join:v2:' "$plain" || true)"
if [ -z "$plain_key" ]; then
  fail "node-a1-overlay minted no --plain key for the positive control: $(cat "$plain")"
  finish
  exit 1
fi
if echo "$plain_key" | grep -qE "$ADDR_RE"; then
  pass "the positive control: the same regex finds an address in a --plain v2 key, so the two checks above are measuring something"
else
  fail "the positive control found no address in a --plain key, so the two checks above prove nothing: $plain_key"
fi

# --- open the reply: joins on the spot, no knock, no six digits ------------
opened="$SCRATCH/opened.txt"
printf '%s\n' "$reply" | nxi node-a1-overlay tcr peer invite --reply --stdin --peers "$PEERS" > "$opened" 2>&1 || true
case "$(cat "$opened")" in
  *"peer invite: ok addr=$B1_OVERLAY"*)
    pass "opening the reply joined immediately, at node-b1-overlay's real address, $B1_OVERLAY" ;;
  *)
    fail "opening the reply did not report a completed join: $(cat "$opened")"
    finish
    exit 1
    ;;
esac
if grep -q "tcr peer pair" "$opened"; then
  fail "a dial-later line was printed; the reply carried a key, so nothing should ask for a second command: $(cat "$opened")"
else
  pass "no dial-later line, no six-digit compare asked for"
fi

# --- both sides pin the other, with no second command ----------------------
a1_ls="$SCRATCH/a1-pinned.json"
b1_ls="$SCRATCH/b1-pinned.json"
ls_json node-a1-overlay "$a1_ls" || true
ls_json node-b1-overlay "$b1_ls" || true
a1_id="$(pinned_wire_id "$b1_ls" || true)"
b1_id="$(pinned_wire_id "$a1_ls" || true)"
if [ -n "$a1_id" ]; then
  pass "node-b1-overlay pins node-a1-overlay after the join, no six-digit compare needed"
else
  fail "node-b1-overlay pins no single Mac after the join: $(cat "$b1_ls")"
fi
if [ -n "$b1_id" ]; then
  pass "node-a1-overlay pinned node-b1-overlay back on the spot, with no second command"
else
  fail "node-a1-overlay pins no single Mac after opening the reply: $(cat "$a1_ls")"
fi

finish
