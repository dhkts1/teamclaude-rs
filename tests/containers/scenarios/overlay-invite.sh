#!/bin/sh
# overlay-invite
# An invite over the overlay: two Macs that share a network no NAT sits in.
#
# Cast: node-a1-overlay and node-b1-overlay, each on its home network AND on
# `overlay`. The overlay stands in for a mesh VPN's reachability and nothing
# else: no WireGuard, no key exchange, no MagicDNS, no ACLs.
#
# Steps
#   1. up both overlay nodes and both routers
#   2. on node-a1-overlay: tcr peer invite
#   3. on node-b1-overlay: tcr peer join <key>
#
# Assertions
#   - the key's FIRST address is the overlay one (10.99.0.11:7755), ranked ahead
#     of the LAN address, because that is the one a friend elsewhere can dial
#   - node-b1 pairs, and the pinned row it writes carries the overlay address
#
# Today: `pair::dial_addresses` ranks what it finds. Whether an overlay
# interface outranks a LAN interface is what this measures; if it does not,
# this is the scenario that says so.
set -eu

HERE="$(cd "$(dirname "$0")" && pwd)"
SCENARIO="overlay-invite"
export SCENARIO
. "$HERE/../lib/assert.sh"

# The compose services this scenario needs, for the runner and for a reader.
SERVICES="node-a1-overlay node-b1-overlay router-a router-b"
export SERVICES

not_wired "$SERVICES: the steps above are written, the driving is not"
