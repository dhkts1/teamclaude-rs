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

# The compose services this scenario needs, for the runner and for a reader.
SERVICES="node-a1"
export SERVICES

not_wired "$SERVICES: the steps above are written, the driving is not"
