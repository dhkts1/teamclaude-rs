#!/bin/sh
# hello-no-wildcard-endpoint
# No row anywhere records 0.0.0.0, and no row records an ephemeral source port.
#
# Cast: node-a1 and node-a2 on home-a, paired.
#
# Steps
#   1. pair the two as in lan-discover-and-pair
#   2. on node-a2: tcr peer hello node-a1
#   3. on both: tcr peer ls --json
#
# Assertions
#   - no address field in either peers file starts with 0.0.0.0
#   - every recorded endpoint's port is the peer's LISTEN port, not the source
#     port of the connection that carried the hello
#
# Today: this is the defect class the invite fix was about, so it is the
# scenario most likely to go red on a regression and the one worth having first.
set -eu

HERE="$(cd "$(dirname "$0")" && pwd)"
SCENARIO="hello-no-wildcard-endpoint"
export SCENARIO
. "$HERE/../lib/assert.sh"

# The compose services this scenario needs, for the runner and for a reader.
SERVICES="node-a1 node-a2"
export SERVICES

not_wired "$SERVICES: the steps above are written, the driving is not"
