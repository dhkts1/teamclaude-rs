#!/bin/sh
# nat-no-mapping
# An invite key across two NATs, with a router that refuses NAT-PMP.
#
# Cast: node-a1 behind router-a (UPNP=off), node-b1 behind router-b, upstream.
#
# Steps
#   1. up router-a router-b node-a1 node-b1 with ROUTER_A_UPNP=off
#   2. on node-a1: tcr peer internet on, then tcr peer reach
#   3. on node-a1: tcr peer invite, capture the key
#   4. on node-b1: tcr peer join <key>
#
# Assertions
#   - node-a1's `tcr peer reach` prints the no-mapping line, naming the refusal
#     rather than a silence (the gateway answers 5351 with ICMP unreachable,
#     which is exactly the owner's router)
#   - the log carries `peer internet: the router would not map this node's
#     listener port`
#   - node-b1's join FAILS, and says the address it could not reach
#
# Today: expected green in the sense that the failure is the correct one. The
# value is the second half: that the refusal is legible and does not retry
# every five seconds forever.
set -eu

HERE="$(cd "$(dirname "$0")" && pwd)"
SCENARIO="nat-no-mapping"
export SCENARIO
. "$HERE/../lib/assert.sh"

# The compose services this scenario needs, for the runner and for a reader.
SERVICES="node-a1 node-b1 router-a router-b"
export SERVICES

not_wired "$SERVICES: the steps above are written, the driving is not"
