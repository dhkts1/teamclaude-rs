#!/bin/sh
# sealed-invite
# Mode B end to end: an ask that names no address, a reply that names no
# address either, and the address regex that proves the claim can fire at
# all.
#
# Cast: node-a1-overlay (10.77.1.11 on home-a) and node-a2 (10.77.1.12 on
# home-a), never paired before this scenario runs. Both share home-a so the
# knock at the end of mode B is admitted: `listener::internet_admission`
# refuses a knock or a first pairing (`NN`/`XX`) from any source outside LAN
# scope, on purpose (`src/peer/listener.rs`, `is_lan_scope_v4`), and
# `100.64.0.0/10` is deliberately NOT LAN scope there, so a tailnet or
# overlay address cannot complete THIS path either, only an `IK`/`IKpsk1`
# return visit or a join key can. A scenario that put the two Macs on
# genuinely separate networks could prove the ask and the reply carry no
# address, but could not run `pair_nodes` to completion, which is why this
# cast is `node-a2`'s: the same one `moved-link` and
# `hello-no-wildcard-endpoint` already pair over.
#
# Steps
#   1. up node-a1-overlay node-a2
#   2. on node-a1-overlay: tcr peer invite --sealed, the ask
#   3. on node-a2: tcr peer join --stdin fed the ask, the reply
#   4. on node-a1-overlay: tcr peer invite --reply --stdin fed the reply, the
#      address it names and the tcr peer pair line
#   5. on node-a1-overlay: tcr peer invite --plain, a v2 key, minted in the
#      same run, the positive control
#   6. pair_nodes node-a1-overlay node-a2 at the address the reply named: the
#      knock, the accept and the six digits, the same path every other join
#      ends at
#
# Assertions
#   - the ask names neither node's address
#   - the reply names neither node's address
#   - the same grep over the --plain v2 key finds node-a2's address: the
#     positive control that proves the grep can fire at all
#   - opening the reply names node-a2's real, dialable address
#   - the knock, the accept and the six digits complete over that address
#
# The address regex is deliberately loose (a dotted quad OR a bracketed run of
# hex and colons) rather than matching this scenario's own address by name: a
# check that only knew to look for 10.77.1.12 would pass on a build that
# leaked a THIRD address into the blob.
set -eu

HERE="$(cd "$(dirname "$0")" && pwd)"
SCENARIO="sealed-invite"
export SCENARIO
# shellcheck source=../lib/assert.sh
. "$HERE/../lib/assert.sh"
# shellcheck source=../lib/compose.sh
. "$HERE/../lib/compose.sh"

SERVICES="node-a1-overlay node-a2"
export SERVICES

A2_HOME=10.77.1.12

# A dotted-quad IPv4 address, or a bracketed run of hex and colons (an IPv6
# one): what a person reading a chat window would recognise as "an address",
# the same shape the design's own contract names.
ADDR_RE='[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+|\[[0-9a-fA-F:]+\]'

# shellcheck disable=SC2086 # SERVICES is a deliberate list of service names
up $SERVICES || { finish; exit 1; }

# --- mint an ask -----------------------------------------------------------
asked="$SCRATCH/asked.txt"
nx node-a1-overlay tcr peer invite --sealed --peers "$PEERS" > "$asked" 2>&1 || true
ask="$(grep -m1 '^tcr-invite:v1:' "$asked" || true)"
if [ -z "$ask" ]; then
  fail "node-a1-overlay minted no ask: $(cat "$asked")"
  finish
  exit 1
fi
pass "node-a1-overlay minted an ask: $(echo "$ask" | cut -c1-24)..."

if echo "$ask" | grep -qE "$ADDR_RE"; then
  fail "the ask names something that reads as an address: $ask"
else
  pass "the ask names no address (checked with a live grep, not by construction)"
fi

# --- answer it ---------------------------------------------------------------
answered="$SCRATCH/answered.txt"
printf '%s\n' "$ask" | nxi node-a2 tcr peer join --stdin --peers "$PEERS" > "$answered" 2>&1 || true
reply="$(grep -m1 '^tcr-reply:v1:' "$answered" || true)"
if [ -z "$reply" ]; then
  fail "node-a2 answered with no reply: $(cat "$answered")"
  finish
  exit 1
fi
pass "node-a2 answered with a reply: $(echo "$reply" | cut -c1-24)..."

if echo "$reply" | grep -qE "$ADDR_RE"; then
  fail "the reply names something that reads as an address: $reply"
else
  pass "the reply names no address either (checked with a live grep)"
fi

# --- the positive control: the same grep over a --plain key finds one ------
# Minted in this same run, on this same Mac, so the control is over the exact
# grep and the exact regex the two checks above just passed under, not a
# hand-picked example from somewhere else.
plain="$SCRATCH/plain.txt"
nx node-a1-overlay tcr peer invite --plain --peers "$PEERS" > "$plain" 2>&1 || true
plain_key="$(grep -m1 '^tcr-join:v2:' "$plain" || true)"
if [ -z "$plain_key" ]; then
  fail "node-a1-overlay minted no --plain key for the positive control: $(cat "$plain")"
  finish
  exit 1
fi
if echo "$plain_key" | grep -qE "$ADDR_RE"; then
  pass "the positive control: the same regex finds an address in a --plain v2 key, so the two checks above are measuring something"
else
  fail "the positive control found no address in a --plain key, so the two checks above prove nothing: $plain_key"
fi

# --- open the reply ----------------------------------------------------------
opened="$SCRATCH/opened.txt"
printf '%s\n' "$reply" | nxi node-a1-overlay tcr peer invite --reply --stdin --peers "$PEERS" > "$opened" 2>&1 || true
case "$(cat "$opened")" in
  *"they answer at $A2_HOME:$LISTEN_PORT"*)
    pass "opening the reply names node-a2's real address, $A2_HOME:$LISTEN_PORT" ;;
  *)
    fail "opening the reply did not name node-a2's address: $(cat "$opened")"
    finish
    exit 1
    ;;
esac
case "$(cat "$opened")" in
  *"tcr peer pair $A2_HOME:$LISTEN_PORT"*)
    pass "the next command to run is printed: tcr peer pair $A2_HOME:$LISTEN_PORT" ;;
  *) fail "the next command to run was not printed: $(cat "$opened")" ;;
esac

# --- the dial that follows: the same knock, accept and six digits ----------
# Mode B ends here, in the shape the design's own contract states: a paste
# alone trusts nothing, and this is the compare that does.
pair_nodes node-a1-overlay node-a2 "$A2_HOME:$LISTEN_PORT" || { finish; exit 1; }

finish
