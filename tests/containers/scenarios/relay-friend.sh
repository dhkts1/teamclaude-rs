#!/bin/sh
# relay-friend
# Two friends pair while they are on the same open network. One of them goes
# home behind a NAT. The friend who stayed reachable carries the bytes.
#
# This is the only scenario in this directory that changes its own topology
# after boot. The reason is that a pin needs one live direct socket to form,
# so the pairing window has to exist before the NAT does: node-b1 boots with
# a second leg on the open lan, the three Macs pair over real sockets while
# that leg is up, and only then does the scenario take it down inside
# node-b1's own namespace. Everything that follows runs against the NAT that
# is left once the window has closed.
#
# Cast: node-a1 (behind router-a, no mapping), node-b1 (behind router-b, no
# mapping, plus a second leg straight onto the open lan that this scenario
# closes mid-run), node-c1 (on the open lan, reachable by both).
#
# Steps
#   1. bring up both routers and all three nodes
#   2. pair node-a1 with node-c1, node-b1 with node-c1, and node-a1 with
#      node-b1, all over real sockets while node-b1's open leg is still up
#      (the invite/join shape lease-borrow.sh:60-87 already drives)
#   3. set the grants a relay needs: node-a1 may ask node-c1 to carry and
#      discloses to node-b1; node-b1 lends to node-a1 and allows it to
#      inspect; node-c1 allows node-a1 to forward through it
#   4. take node-b1's open-lan leg down, inside its own namespace
#   5. the negative control: a real socket connect from node-a1 to
#      node-b1's (now down) open-lan address fails
#   6. disable node-a1's own account and restart it, then drive one request
#      through its own proxy (lease-borrow.sh:125-163 is the model)
#   7. node-a1's boot log carries the dialler's own `via=` line
#      (src/peer/serve.rs:2563-2567), naming node-c1 as the Mac that carried,
#      AND the lease request that rode that stream did not fail
#
# There is no `kind=Via` assertion here. Production never constructs
# `Endpoint::via` (every caller is a test fixture), so no row can ever show
# one from a real forward.
#
# `via=` alone is not enough either, measured directly below: `open_forward_to`
# (src/peer/tunnel.rs) returns a stream the instant it has ASKED node-c1 to
# carry, before node-c1 has said yes, so `via=` prints whether node-c1
# authorizes the forward or not. The second assertion is the one that actually
# depends on the grant: a node-c1 that refuses closes the carried stream
# instead of reaching node-b1 with it, and the lease request riding that
# stream fails right there with "the handshake with the lender failed".
set -eu

HERE="$(cd "$(dirname "$0")" && pwd)"
SCENARIO="relay-friend"
export SCENARIO
# shellcheck source=../lib/assert.sh
. "$HERE/../lib/assert.sh"

if [ "${NETLAB:-0}" != "1" ]; then
  not_wired "compose: this scenario manipulates its own topology mid-run and only the netlab harness can do that"
fi

TOPOLOGY="$HERE/../topologies/three-homes-one-open.json"
export TOPOLOGY
NETLAB_UPSTREAM="http://5.5.5.99:8080"
export NETLAB_UPSTREAM
# shellcheck source=../lib/netlab.sh
. "$HERE/../lib/netlab.sh"

B_OPEN_ADDR=5.5.5.11
C_ADDR=5.5.5.20
ROUTER_A_OUTSIDE=5.5.5.2

# id_by_addr_prefix <ls-json-file> <ip-prefix-with-trailing-colon>
#
# A joiner's own row for its registrar carries no label this scenario chose
# (`registrar_label` derives `peer-<port>` from the port alone, and every
# node here listens on the same port, so two joins from one Mac collide on
# the same default label). The endpoint address never collides the same way,
# so every id below is read off the row that answers at the address this
# scenario already knows, on the host side, past `nx`, the same place
# `pinned_wire_id` already reads a listing from.
id_by_addr_prefix() {
  file="$1"
  prefix="$2"
  python3 -c "
import json
data = json.load(open('$file'))
prefix = '$prefix'
found = ''
for row in data.get('peers', []) or []:
    for ep in row.get('endpoints', []) or []:
        if ep.get('kind') == 'direct' and ep.get('addr', '').startswith(prefix):
            found = row.get('node', '')
            break
    if found:
        break
print(found)
"
}

# pair_by_invite <minter> <joiner>
#
# The joiner's own name is the label on both ends: the minter's `--label`
# names the Mac it expects to join (lease-borrow.sh's convention), and the
# joiner's own `--label` is the name it hands the registrar over the wire
# (src/peer/pair.rs:927-930). They are the same string because both
# questions have the same honest answer, the joiner's own name.
pair_by_invite() {
  minter="$1"
  joiner="$2"
  minted="$SCRATCH/minted-$minter-$joiner.txt"
  nx "$minter" tcr peer invite --peers "$PEERS" --label "$joiner" > "$minted" 2>&1 || true
  token="$(grep -m1 '^tcr-join:' "$minted" || true)"
  if [ -z "$token" ]; then
    fail "$minter minted no invite key for $joiner: $(cat "$minted")"
    finish
    exit 1
  fi
  joined="$(printf '%s\n' "$token" | nxi "$joiner" tcr peer join --stdin --peers "$PEERS" --label "$joiner" 2>&1 || true)"
  case "$joined" in
    *"peer join: ok"*) pass "$joiner joined $minter over a real socket: $joined" ;;
    *) fail "$joiner's join with $minter did not report ok: $joined"; finish; exit 1 ;;
  esac
}

up_with_routers router-a router-b -- node-a1 node-b1 node-c1 || { finish; exit 1; }
pass "router-a router-b node-a1 node-b1 node-c1 all reported up"

# --- pair, all three, while node-b1's open leg is still up -------------------
pair_by_invite node-c1 node-a1
pair_by_invite node-c1 node-b1
pair_by_invite node-b1 node-a1

a_ls="$SCRATCH/a-ls.json"
b_ls="$SCRATCH/b-ls.json"
c_ls="$SCRATCH/c-ls.json"
ls_json node-a1 "$a_ls" || true
ls_json node-b1 "$b_ls" || true
ls_json node-c1 "$c_ls" || true

c_id_from_a="$(id_by_addr_prefix "$a_ls" "$C_ADDR:")"
b_id_from_a="$(id_by_addr_prefix "$a_ls" "$B_OPEN_ADDR:")"
a_id_from_b="$(id_by_addr_prefix "$b_ls" "$ROUTER_A_OUTSIDE:")"
c_id_from_b="$(id_by_addr_prefix "$b_ls" "$C_ADDR:")"
a_id_from_c="$(id_by_addr_prefix "$c_ls" "$ROUTER_A_OUTSIDE:")"
# node-b1 dials node-c1 straight over the open lan, no NAT in the way (both
# sit on the same bridge), so node-c1 sees node-b1's OWN open-lan address
# rather than router-b's outside leg, unlike node-a1, which always answers
# through its router's NAT.
b_id_from_c="$(id_by_addr_prefix "$c_ls" "$B_OPEN_ADDR:")"

if [ -z "$c_id_from_a" ] || [ -z "$b_id_from_a" ] || [ -z "$a_id_from_b" ] || [ -z "$c_id_from_b" ] || [ -z "$a_id_from_c" ] || [ -z "$b_id_from_c" ]; then
  fail "one of the six pinned ids did not resolve (c_from_a=$c_id_from_a b_from_a=$b_id_from_a a_from_b=$a_id_from_b c_from_b=$c_id_from_b a_from_c=$a_id_from_c b_from_c=$b_id_from_c)"
  finish
  exit 1
fi
pass "every pinned row resolved to exactly one id on each side"

# --- the grant matrix, derived from src/peer/serve.rs:2658 and
#     src/peer/tunnel.rs:1353 -------------------------------------------------
nx node-a1 tcr peer allow "$c_id_from_a" carry on --peers "$PEERS" >/dev/null 2>&1 || true
nx node-a1 tcr peer allow "$b_id_from_a" disclose on --peers "$PEERS" >/dev/null 2>&1 || true
nx node-b1 tcr peer allow "$a_id_from_b" inspect on --peers "$PEERS" >/dev/null 2>&1 || true
lend_out="$(nx node-b1 tcr peer lend "$a_id_from_b" --fraction 0.5 --ttl 600 --max-inflight 2 --scope all --peers "$PEERS" --config /lab/node-b1/.config/teamclaude.json 2>&1 || true)"
case "$lend_out" in
  *"peer lend: ok"*) pass "node-b1 lent node-a1 a lease: $(echo "$lend_out" | head -1)" ;;
  *) fail "node-b1's lend to node-a1 did not report ok: $lend_out" ;;
esac
nx node-c1 tcr peer allow "$a_id_from_c" forward on --peers "$PEERS" >/dev/null 2>&1 || true
pass "node-a1 may carry through node-c1 and discloses to node-b1; node-b1 lends to node-a1 and inspects for it; node-c1 forwards for node-a1"

# --- node-b1 goes home: its open-lan leg comes down inside its own
#     namespace ---------------------------------------------------------------
b_open_if="$(ip netns exec node-b1 python3 -c "
import subprocess
out = subprocess.check_output(['ip', '-o', '-4', 'addr', 'show']).decode()
found = ''
for line in out.splitlines():
    parts = line.split()
    if len(parts) >= 4 and parts[3].startswith('$B_OPEN_ADDR/'):
        found = parts[1]
        break
print(found)
" 2>/dev/null | tr -d '\r\n')"
if [ -z "$b_open_if" ]; then
  fail "could not find node-b1's own interface holding $B_OPEN_ADDR to take down"
  finish
  exit 1
fi
ip netns exec node-b1 ip link set "$b_open_if" down
pass "node-b1's open-lan leg ($b_open_if, $B_OPEN_ADDR) is down; node-b1 stays up on home-b"

# --- the negative control, before the relay leg runs: a real socket connect,
#     not a restatement of the `ip link set down` above --------------------
neg_out="$(ip netns exec node-a1 python3 -c "
import socket
s = socket.socket()
s.settimeout(3.0)
try:
    s.connect(('$B_OPEN_ADDR', $LISTEN_PORT))
except OSError as err:
    print('relay-friend: negative control: connect to $B_OPEN_ADDR:$LISTEN_PORT failed: ' + str(err))
else:
    s.close()
    print('relay-friend: negative control: connect to $B_OPEN_ADDR:$LISTEN_PORT unexpectedly succeeded')
" 2>&1 || true)"
case "$neg_out" in
  *"unexpectedly succeeded"*)
    fail "node-a1 can still reach node-b1 directly at $B_OPEN_ADDR:$LISTEN_PORT after its open leg came down: $neg_out" ;;
  *"failed:"*)
    pass "node-a1 cannot reach node-b1 directly any more, a real connect attempt, not the ip-link-down step restated: $neg_out" ;;
  *)
    fail "node-a1's direct-reach probe produced neither a failure nor a success: $neg_out" ;;
esac

# --- disable node-a1's own account, restart it, so the peer-lease fallback
#     provider is the only thing that can answer a real request -------------
nx node-a1 tcr disable node-a1-fake --config /lab/node-a1/.config/teamclaude.json >/dev/null 2>&1 || true
oldpid="$(cat /lab/node-a1/pid 2>/dev/null || echo)"
if [ -n "$oldpid" ]; then
  kill "$oldpid" 2>/dev/null || true
fi
sleep 0.5
echo "--- relay-friend: node-a1 restarted with its own account disabled ---" >> /lab/node-a1/boot.log
nx node-a1 tcr --headless --port 8088 --no-replace >> /lab/node-a1/boot.log 2>&1 &
echo $! > /lab/node-a1/pid
if ! wait_for_log node-a1 "peer listener up" 30 >/dev/null; then
  fail "node-a1 never came back up after the restart"
  finish
  exit 1
fi
pass "node-a1 restarted with its own account disabled"

# --- the borrow itself: one request through node-a1's own proxy, with no
#     local account able to answer it ----------------------------------------
# The punch has to fail before the forwarder fallback runs
# (`dial_peer_reaching_within` tries `punch_dial` first, and neither router
# has a mapping), so this one request is the slow part of the whole scenario.
borrow="$(nx node-a1 env HOME=/lab/node-a1 python3 "$SRC/netlab-client.py" /lab/node-a1/.config/teamclaude.json 1 128 1 2>&1 || true)"
echo "relay-friend: node-a1's borrow request: $borrow"

# --- the observable, in two parts ---------------------------------------
#
# `open_forward_to` (src/peer/tunnel.rs) never waits for the forwarder's
# answer before returning a stream (its own doc comment: "nothing here waits
# for an acknowledgement"), so the `via=` line alone fires the instant this
# Mac finishes asking node-c1 to carry, whether node-c1 actually authorizes
# the forward or not: measured directly, with `tcr peer allow <node-a1>
# forward off` set on node-c1, `via=` still printed on node-a1 and the very
# next line was `peer lease: asking for a lease failed ... the handshake
# with the lender failed`, because node-c1 closed the carried stream instead
# of dialling node-b1 with it. So the gate is the PAIR: `via=` on its own
# would have passed the forced red below too.
wait_for_log node-a1 'via=' 90 >/dev/null || true
expect_log node-a1 'via=' "node-a1's log carries the dialler's own via= line, naming the Mac that carried (src/peer/serve.rs:2563-2567)"
refute_log node-a1 'peer lease: asking for a lease failed' "the lease request that rode that stream did not fail (a forwarder that refused the carry closes it instead of reaching node-b1, and the request fails right there)"

finish
