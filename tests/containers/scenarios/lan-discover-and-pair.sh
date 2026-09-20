#!/bin/sh
# lan-discover-and-pair
# Two Macs on one home LAN find each other and end up trusted.
#
# Cast: node-a1 and node-a2 on home-a, both with FIND=on.
#
# Steps
#   1. up node-a1 node-a2, wait for `peer listener up` in each boot log
#   2. on node-a2: tcr peer pair 10.77.1.11:7755
#   3. on node-a1: tcr peer pending, then tcr peer accept <instance>
#   4. read the six-digit compare off both sides and confirm it matches
#   5. on both: tcr peer ls --json
#
# Assertions
#   - both beacons answer a PTR query for _tcr-peer._tcp.local (mdns-observe.py)
#   - node-a1's `pending` holds exactly one knock, naming node-a2
#   - after accept, each side's `peers[]` holds the other's node id
#
# Today: the pair leg is green in tests/peer_e2e.rs on one host. What is new
# here is the beacon crossing a real bridge, which the probe already showed it
# does, and a pin whose address is the peer's LAN address rather than loopback.
set -eu

HERE="$(cd "$(dirname "$0")" && pwd)"
SCENARIO="lan-discover-and-pair"
export SCENARIO
. "$HERE/../lib/assert.sh"

# The compose services this scenario needs, for the runner and for a reader.
SERVICES="node-a1 node-a2"
export SERVICES

not_wired "$SERVICES: the steps above are written, the driving is not"
