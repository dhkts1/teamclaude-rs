#!/bin/sh
# lease-borrow
# One Mac lends, the other borrows, and a real request is served.
#
# Cast: node-a1 (lender, one working fake account), node-a2 (borrower, its own
# account disabled), upstream.
#
# Steps
#   1. pair the two
#   2. on node-a1: tcr peer lend <node-a2-id>, and allow the grants it needs
#   3. restart node-a2 (the peer-lease provider installs at boot)
#   4. from the host: POST /v1/messages at node-a2's published proxy port
#
# Assertions
#   - node-a2's log carries `provider=peer-lease` on the line
#     "no local account could serve this request, served through a fallback
#     provider" (src/proxy.rs, src/peer/lease.rs)
#   - the stub upstream's seen log records the LENDER's credential, not the
#     borrower's
#   - the response is a 200 carrying the canned message body
set -eu

HERE="$(cd "$(dirname "$0")" && pwd)"
SCENARIO="lease-borrow"
export SCENARIO
. "$HERE/../lib/assert.sh"

# The compose services this scenario needs, for the runner and for a reader.
SERVICES="node-a1 node-a2 upstream"
export SERVICES

not_wired "$SERVICES: the steps above are written, the driving is not"
