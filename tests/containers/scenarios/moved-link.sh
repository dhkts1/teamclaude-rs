#!/bin/sh
# moved-link
# Mint a moved link, open it, and open it again.
#
# Cast: node-a1 and node-a2 on home-a, already paired.
#
# Steps
#   1. pair the two
#   2. on node-a1: tcr peer moved mint <node-b-id>
#   3. on node-a2: tcr peer moved open --stdin --yes
#   4. on node-a2: tcr peer moved open --stdin --yes, the same link again
#
# Assertions
#   - the first open adds the addresses and says how many
#   - the second open prints `peer moved: already-known` and writes nothing
#   - the peers file's mtime is unchanged by the second open
set -eu

HERE="$(cd "$(dirname "$0")" && pwd)"
SCENARIO="moved-link"
export SCENARIO
. "$HERE/../lib/assert.sh"

# The compose services this scenario needs, for the runner and for a reader.
SERVICES="node-a1 node-a2"
export SERVICES

not_wired "$SERVICES: the steps above are written, the driving is not"
