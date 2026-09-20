#!/bin/sh
# knock-nondefault-port
# A Mac listening somewhere other than 7755 is paired back on that port.
#
# Cast: node-a1 with PEER_LISTEN=0.0.0.0:7788, node-a2 default.
#
# Steps
#   1. up both, node-a1 with PEER_LISTEN=0.0.0.0:7788
#   2. on node-a2: tcr peer pair 10.77.1.11:7788
#   3. accept on node-a1
#   4. on node-a2: tcr peer ls --json
#
# Assertions
#   - node-a2's pinned row for node-a1 carries :7788, never :7755
#   - node-a1's knock-accepted line names the address the knock came from
#
# Today: expected RED or flaky. The port a knock is answered on comes from the
# peers file's `listen`, and the pin written by the compare path records no
# address at all (`pair::confirm`, the gap tests/peer_e2e.rs names first), so
# a row pinned this way may have nothing to dial on any port.
set -eu

HERE="$(cd "$(dirname "$0")" && pwd)"
SCENARIO="knock-nondefault-port"
export SCENARIO
. "$HERE/../lib/assert.sh"

# The compose services this scenario needs, for the runner and for a reader.
SERVICES="node-a1 node-a2"
export SERVICES

not_wired "$SERVICES: the steps above are written, the driving is not"
