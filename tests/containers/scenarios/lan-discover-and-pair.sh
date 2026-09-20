#!/bin/sh
# lan-discover-and-pair
# Two Macs on one home LAN find each other and end up trusted.
#
# Cast: node-a1 (10.77.1.11) and node-a2 (10.77.1.12) on home-a, both with
# FIND=on set before their servers boot, plus the observer on the same bridge.
#
# Steps
#   1. up node-a1 node-a2, wait for `peer listener up` in each boot log
#   2. ask the wire: one multicast PTR query for _tcr-peer._tcp.local
#   3. before any pair: each node's `tcr peer ls --json` shows the OTHER as
#      found, and neither shows itself
#   4. node-a2 pairs at 10.77.1.11:7755; node-a1 accepts; compare; confirm
#   5. node-a1 pairs back at 10.77.1.12:7755; node-a2 accepts; compare; confirm
#   6. on both: tcr peer ls --json
#
# Assertions
#   - both beacons answer the PTR query, one from each node's own address
#   - before any pair, each node's `peers` array carries the other as a found
#     row (`trusted: false`, no `node`), and carries neither node's own address
#   - the two screens show the SAME six digits, each way round
#   - each side's peers file pins the OTHER node's id, and nothing else
#   - the pin carries the peer's LAN address and its listen port, not loopback
#
# The serving process browses on its own 20 s cadence beside its beacon and
# writes what it heard into `peer-state.json`; `tcr peer ls --json` reads that
# file and shows an unpinned Mac as a found row, `trusted: false`, no `node`.
# This used to be false: a discovered node never appeared in that listing at
# all, and this scenario's own step 3 is what changed once it did.
#
# Both directions for the pairing, because a pairing writes a row only on the
# side that ran `tcr peer pair`. The accepting side deliberately writes none,
# which is why the CLI's own closing line tells the operator to run it on the
# other Mac too.
set -eu

HERE="$(cd "$(dirname "$0")" && pwd)"
SCENARIO="lan-discover-and-pair"
export SCENARIO
# shellcheck source=../lib/assert.sh
. "$HERE/../lib/assert.sh"

if [ "${NETLAB:-0}" = "1" ]; then
  TOPOLOGY="$HERE/../topologies/two-on-one-lan.json"
  export TOPOLOGY
  # shellcheck source=../lib/netlab.sh
  . "$HERE/../lib/netlab.sh"
  NODES="node-a1 node-a2"
  # shellcheck disable=SC2086 # NODES is a deliberate list of node names
  up $NODES || { finish; exit 1; }
else
  # shellcheck source=../lib/compose.sh
  . "$HERE/../lib/compose.sh"

  # The compose services this scenario needs, for the runner and for a reader.
  SERVICES="node-a1 node-a2"
  export SERVICES

  # Announced before either server boots, so the beacon starts with the server
  # rather than within the twenty seconds a running one takes to re-read the flag.
  NODE_A1_FIND=on
  NODE_A2_FIND=on
  NODE_A1_ANNOUNCE_NAME=on
  NODE_A2_ANNOUNCE_NAME=on
  export NODE_A1_FIND NODE_A2_FIND NODE_A1_ANNOUNCE_NAME NODE_A2_ANNOUNCE_NAME

  # shellcheck disable=SC2086 # SERVICES is a deliberate list of service names
  up $SERVICES || { finish; exit 1; }
fi

# --- the beacons, off the wire -------------------------------------------
seen="$SCRATCH/mdns.txt"
if [ "${NETLAB:-0}" = "1" ]; then
  observe observer 10 > "$seen" 2>&1 || true
else
  dc run --rm -T --no-deps observer > "$seen" 2>&1 || true
fi
cat "$seen"
for address in 10.77.1.11 10.77.1.12; do
  if grep -q "^mdns: $address: " "$seen"; then
    pass "a beacon answered from $address: $(grep -m1 "^mdns: $address: " "$seen")"
  else
    fail "no beacon answered from $address within the observer's window"
  fi
done

# --- each node sees the other as found, before any pairing runs ----------
a1_found_ls="$SCRATCH/a1-found.json"
a2_found_ls="$SCRATCH/a2-found.json"

# The serving process browses on its own 20s cadence beside its beacon, so a
# listing read the instant `up` returns can predate the row this scenario
# asserts on. This waits for the row, on the same deadline shape pair_nodes
# already polls with, instead of assuming a browse has happened.
check_found() {
  # $1 the node whose file this is, $2 the listing path to (re)write, $3 the
  # node it must see as found, $4 the address and name that node announces
  # (`addr name`).
  who="$1"
  listing="$2"
  other="$3"
  wanted="$4"
  found="$SCRATCH/$who-found.txt"
  i=0
  while [ "$i" -lt 50 ]; do
    ls_json "$who" "$listing" || true
    python3 "$LIB/peer-read.py" found < "$listing" > "$found" 2>&1 || true
    if grep -qF "$wanted" "$found"; then
      pass "$who sees $other as found, not trusted: $wanted"
      return
    fi
    sleep 0.5
    i=$((i + 1))
  done
  fail "$who does not see $other as found within 25s of a 20s browse cadence (found: [$(tr '\n' ';' < "$found")])"
}

check_no_self_found() {
  # Positive control: an own-beacon filter that stopped working would put
  # this node's own address in its own found list. $1 the node, $2 its
  # listing, $3 its own address.
  who="$1"
  listing="$2"
  own_address="$3"
  found="$SCRATCH/$who-found.txt"
  if grep -q "^$own_address " "$found"; then
    fail "$who sees ITSELF as found at $own_address; the own-instance filter did not hold"
  else
    pass "$who does not see itself as found (own-instance filter holds)"
  fi
}

check_found node-a1 "$a1_found_ls" node-a2 "10.77.1.12:$LISTEN_PORT node-a2"
check_no_self_found node-a1 "$a1_found_ls" "10.77.1.11:$LISTEN_PORT"
check_found node-a2 "$a2_found_ls" node-a1 "10.77.1.11:$LISTEN_PORT node-a1"
check_no_self_found node-a2 "$a2_found_ls" "10.77.1.12:$LISTEN_PORT"

# --- the two pairings ----------------------------------------------------
pair_nodes node-a2 node-a1 "10.77.1.11:$LISTEN_PORT" || { finish; exit 1; }
pair_nodes node-a1 node-a2 "10.77.1.12:$LISTEN_PORT" || { finish; exit 1; }

# --- what each file now holds --------------------------------------------
a1_ls="$SCRATCH/a1.json"
a2_ls="$SCRATCH/a2.json"
ls_json node-a1 "$a1_ls" || true
ls_json node-a2 "$a2_ls" || true

check_pin() {
  # $1 the node whose file this is, $2 its listing, $3 the node whose id it
  # must hold, $4 the address that node answers on.
  who="$1"
  listing="$2"
  other="$3"
  address="$4"
  wanted="$(pinned_wire_id "$listing" || true)"
  if [ -z "$wanted" ]; then
    fail "$who does not pin exactly one Mac: [$(python3 "$LIB/peer-read.py" nodes < "$listing" 2>&1 | tr '\n' ' ')]"
    return
  fi
  # Held against the id the OTHER container prints for itself, so the row is
  # checked against that Mac's own key rather than against this scenario's
  # idea of which service is which.
  theirs="$(own_short_id "$other" || true)"
  if [ "$(short_of "$wanted")" = "$theirs" ]; then
    pass "$who pins $other and nothing else ($wanted, which $other calls itself)"
  else
    fail "$who pins $wanted, which is not $other ($theirs)"
    return
  fi
  endpoints="$SCRATCH/$who-endpoints.txt"
  if ! python3 "$LIB/peer-read.py" endpoints "$wanted" < "$listing" > "$endpoints" 2>&1; then
    fail "$who: no endpoints readable for $wanted: $(cat "$endpoints")"
    return
  fi
  if grep -q "^direct $address\$" "$endpoints"; then
    pass "$who reaches $other at $address, its LAN address and its listen port"
  else
    fail "$who holds [$(tr '\n' ' ' < "$endpoints")] for $other, not $address"
  fi
}

check_pin node-a1 "$a1_ls" node-a2 "10.77.1.12:$LISTEN_PORT"
check_pin node-a2 "$a2_ls" node-a1 "10.77.1.11:$LISTEN_PORT"

finish
