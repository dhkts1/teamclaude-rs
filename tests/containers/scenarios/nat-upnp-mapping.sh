#!/bin/sh
# nat-upnp-mapping
# The same two NATs, with UPnP available on the inviter's router.
#
# Cast: as nat-no-mapping, but ROUTER_A_UPNP=on.
#
# Steps
#   1. up with ROUTER_A_UPNP=on
#   2. on node-a1: tcr peer internet on, then tcr peer reach
#   3. on node-a1: tcr peer invite, capture the key
#   4. on node-b1: tcr peer join <key>
#   5. on router-a: miniupnpd's lease list
#
# Assertions
#   - `tcr peer reach` prints an external address on router-a's outside address
#   - the mapping shows up in miniupnpd's leases for the node's listen port
#   - node-b1 pairs, and its pinned row for node-a1 carries the MAPPED port
#
# Today: the fallback from a refused NAT-PMP to UPnP is in this tree
# (`MappingKeeper::map`, the `no_natpmp_service` arm). Whether miniupnpd's own
# refusals are legible from the node side is the open question this answers.
set -eu

HERE="$(cd "$(dirname "$0")" && pwd)"
SCENARIO="nat-upnp-mapping"
export SCENARIO
. "$HERE/../lib/assert.sh"

# The compose services this scenario needs, for the runner and for a reader.
SERVICES="node-a1 node-b1 router-a router-b"
export SERVICES

not_wired "$SERVICES: the steps above are written, the driving is not"
