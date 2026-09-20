#!/bin/sh
# state-restored-log-spam
# One node, sixty seconds, and a log that stays quiet.
#
# Cast: node-a1 alone.
#
# Steps
#   1. up node-a1
#   2. wait 60 s
#   3. count `peer state restored` lines in its boot log
#
# Assertions
#   - at most two `peer state restored` lines in the window (src/peer/state.rs)
#   - the count is printed either way, so a regression says how bad it got
#
# Today: a quiet-log assertion is the one a scenario harness is uniquely good
# at, because it needs a process left alone for a minute rather than a unit
# test.
set -eu

HERE="$(cd "$(dirname "$0")" && pwd)"
SCENARIO="state-restored-log-spam"
export SCENARIO
. "$HERE/../lib/assert.sh"

if [ "${NETLAB:-0}" != "1" ]; then
  not_wired "compose: the steps above are written, the driving is only under --netlab"
fi

TOPOLOGY="$HERE/../topologies/one-node.json"
export TOPOLOGY
# shellcheck source=../lib/netlab.sh
. "$HERE/../lib/netlab.sh"

up node-a1 || { finish; exit 1; }
pass "node-a1 is up"

sleep 60

count="$(grep -c "peer state restored" "/lab/node-a1/boot.log" 2>/dev/null || true)"
count="${count:-0}"
echo "state-restored-log-spam: node-a1 printed 'peer state restored' $count time(s) in 60s"
if [ "$count" -le 2 ]; then
  pass "at most two 'peer state restored' lines in 60s ($count)"
else
  fail "$count 'peer state restored' lines in 60s, more than the two expected"
fi

finish
