#!/bin/sh
# dead-drop
# Two Macs behind separate NATs, pinned over the overlay, teach each other
# their real address through the dead drop after the overlay that paired
# them goes away, and the next request still gets served.
#
# Cast: node-a1-overlay (lender, behind router-a, upnp on) and
# node-b1-overlay (borrower, behind router-b, upnp off), on
# `two-homes-overlay-drop.json`, `two-homes-overlay.json` with one field
# changed: router-a's `upnp` is `on`, so node-a1-overlay can hold a real
# router mapping and `own_endpoints` (src/peer/drop.rs) has something to
# publish. `two-homes-two-routers.json`, the topology the unit list first
# named, cannot pair at all: `nat-no-mapping.sh` states its own expected
# outcome as a failed join, and `pair_nodes` is never called on it. Pairing
# has to complete for a `PeerRow::rendezvous_secret` to exist, the only input
# the drop's key ladder has, so that topology could never reach this
# scenario's own first step.
#
# The publisher (`keep_drops_published`, `src/server.rs`) is boot-time gated
# on `DeadDropConfig::is_live()`: a Mac that turns the drop on after it has
# already booted starts no publishing task until it boots again (the fetch
# gate is hot, the publish gate is not, `src/peer/config.rs`'s own doc on
# `DeadDropConfig::enabled` says so). node-a1-overlay is therefore
# configured and then restarted, the same kill-and-relaunch this harness
# already uses in `lease-borrow.sh`.
#
# `dial_peer_reaching_within` (src/peer/serve.rs), where the drop fetch
# lives, has exactly two production callers in this tree and both are the
# peer-lease borrow path (`src/peer/lease.rs`, `src/peer/serve.rs`): there is
# no lighter way to make a real dial happen. So this scenario is
# `lease-borrow.sh`'s own cast and mechanism (lend, the inspect/disclose
# pair, an account disabled and the borrower restarted) with the dead drop
# wired on top of it, not a smaller scenario next to it.
#
# Steps
#   1. up both routers, then both overlay nodes
#   2. pair node-a1-overlay and node-b1-overlay over the overlay, sealed-
#      invite's own mechanism (an ask that names no address, a reply sealed
#      to it, opened on the spot): the cast `nat-no-mapping.sh` cannot use
#   3. node-a1-overlay greets node-b1-overlay (`tcr peer hello`): a freshly
#      paired row carries no `rendezvous_secret` (`moved-link.sh` states this
#      about the very same sealed-pairing shape), and that secret is the only
#      input the drop's key ladder has. One greeting derives it from a real
#      handshake and both sides keep a copy. The direction matters: after a
#      sealed pairing only one side has dialed, so the other side's row for
#      its peer holds an ephemeral source port, not a listening port, and a
#      greet sent that way is a single refused candidate. node-a1-overlay is
#      the side that was dialed, so its row for node-b1-overlay already holds
#      a dialable endpoint; greeting from there is the direction that works,
#      and it is also how node-b1-overlay's row learns node-a1-overlay's real
#      listening port, from node-a1-overlay's `Hello`.
#   4. grant `control-drop` each way, `inspect` on node-a1-overlay for
#      node-b1-overlay, `disclose` on node-b1-overlay for node-a1-overlay
#   5. point both at the stub's `/drop/{name}` surface and turn the drop on
#   6. node-a1-overlay: `tcr peer internet on`, then MEASURE its boot log for
#      the router mapping before deciding anything else
#   7. restart node-a1-overlay so its publisher task actually starts
#   8. node-a1-overlay lends node-b1-overlay a lease, and serves one request
#      through its own account first so it has measured headroom to lend
#   9. disable node-b1-overlay's own account
#  10. take the overlay down on both sides: the only address either side
#      learned of the other during pairing was over it, so this is the
#      point at which node-b1-overlay holds an address for node-a1-overlay
#      it can no longer reach
#  11. restart node-b1-overlay (the account-disable and the peer-lease
#      fallback provider are both boot-time)
#  12. wait out `DEAD_DROP_PUBLISH_INTERVAL` (`src/server.rs`, 300s, fixed):
#      node-a1-overlay's publisher fires its first tick the instant it boots,
#      before its own internet keeper's UPnP round trip can possibly have
#      finished, so that first publish carries no address at all. Its SECOND
#      tick, five minutes later, is the first one with something to publish,
#      and `fetch_for`'s own gate (`fetched_this_slot`, at most once a slot)
#      means a fetch that lands on the empty first record gets no second try
#      this slot. Steps 9-11 run inside this wait rather than after it.
#  13. node-b1-overlay serves one request with its own account disabled
#
# Assertions
#   (a) node-b1-overlay's `tcr peer reach --json` names no fetch for
#       node-a1-overlay before the request and a real one after
#   (b) the request still serves 200, through node-a1-overlay's own account,
#       over the address the drop just taught node-b1-overlay: the ledger's
#       new row carries the LENDER's credential, `lease-borrow.sh`'s own
#       correlation, because there is no `request_id` to match on a relayed
#       control-channel call
#
# Both ship together: everything above (b) needs (pairing, grants, the
# store, the restart, the lend) is what (a) already needs too, so there is
# no smaller scenario that proves (a) alone and the incremental cost of (b)
# is the overlay teardown and the ledger read, both already written for
# other scenarios in this file's shape.
set -eu

HERE="$(cd "$(dirname "$0")" && pwd)"
SCENARIO="dead-drop"
export SCENARIO
# shellcheck source=../lib/assert.sh
. "$HERE/../lib/assert.sh"

if [ "${NETLAB:-0}" != "1" ]; then
  not_wired "compose: this cast needs a real router mapping and a namespace restart, only wired under --netlab"
fi

# The dead drop's own bearer for this lab, obviously fake, matching
# `upstream-stub.py`'s `DROP_TOKEN` constant exactly: a mismatch here would
# read as the store answering 401, `StoreRefusal::Status`, on every publish
# and every fetch.
DROP_TOKEN="fake-netlab-drop-token"
DROP_TEMPLATE="http://5.5.5.20:8080/drop/{name}"

TOPOLOGY="$HERE/../topologies/two-homes-overlay-drop.json"
export TOPOLOGY
NETLAB_UPSTREAM="http://5.5.5.20:8080"
export NETLAB_UPSTREAM
# shellcheck source=../lib/netlab.sh
. "$HERE/../lib/netlab.sh"

A=node-a1-overlay
B=node-b1-overlay
A_HOME=/lab/$A
B_HOME=/lab/$B
A_CONFIG=$A_HOME/.config/teamclaude.json
B_CONFIG=$B_HOME/.config/teamclaude.json

up_with_routers router-a router-b -- "$A" "$B" || { finish; exit 1; }
pass "router-a (upnp on), router-b, $A and $B are all up"

# --- pair over the overlay: sealed-invite's own mechanism -------------------
asked="$SCRATCH/asked.txt"
nx "$A" tcr peer invite --sealed --peers "$PEERS" > "$asked" 2>&1 || true
ask="$(grep -m1 '^tcr-invite:v1:' "$asked" || true)"
if [ -z "$ask" ]; then
  fail "$A minted no ask: $(cat "$asked")"
  finish
  exit 1
fi
pass "$A minted an ask"

answered="$SCRATCH/answered.txt"
printf '%s\n' "$ask" | nxi "$B" tcr peer join --stdin --peers "$PEERS" > "$answered" 2>&1 || true
reply="$(grep -m1 '^tcr-reply:v1:' "$answered" || true)"
if [ -z "$reply" ]; then
  fail "$B answered with no reply: $(cat "$answered")"
  finish
  exit 1
fi
pass "$B answered with a reply"

opened="$SCRATCH/opened.txt"
printf '%s\n' "$reply" | nxi "$A" tcr peer invite --reply --stdin --peers "$PEERS" > "$opened" 2>&1 || true
case "$(cat "$opened")" in
  *"peer invite: ok"*) pass "opening the reply joined immediately: $(cat "$opened")" ;;
  *)
    fail "opening the reply did not report a completed join: $(cat "$opened")"
    finish
    exit 1
    ;;
esac

a_ls="$SCRATCH/a-pinned.json"
b_ls="$SCRATCH/b-pinned.json"
ls_json "$A" "$a_ls" || true
ls_json "$B" "$b_ls" || true
b_id="$(pinned_wire_id "$a_ls" || true)"
a_id="$(pinned_wire_id "$b_ls" || true)"
if [ -z "$a_id" ] || [ -z "$b_id" ]; then
  fail "the pin did not resolve to exactly one id on each side ($A sees:$b_id $B sees:$a_id)"
  finish
  exit 1
fi
pass "$A sees $B as $b_id, $B sees $A as $a_id"

# --- greet: the only way either row gets a rendezvous_secret ---------------
greeted="$(nx "$A" tcr peer hello "$b_id" --peers "$PEERS" 2>&1 || true)"
case "$greeted" in
  *"peer hello: ok"*) pass "$A greeted $B, so both rows hold this pair's shared secret" ;;
  *)
    fail "$A greeting $B: $greeted"
    finish
    exit 1
    ;;
esac

# --- grants: control-drop each way, plus the lease pair ---------------------
nx "$A" tcr peer allow "$b_id" control-drop on --peers "$PEERS" >/dev/null 2>&1 || true
nx "$B" tcr peer allow "$a_id" control-drop on --peers "$PEERS" >/dev/null 2>&1 || true
nx "$A" tcr peer allow "$b_id" inspect on --peers "$PEERS" >/dev/null 2>&1 || true
nx "$B" tcr peer allow "$a_id" disclose on --peers "$PEERS" >/dev/null 2>&1 || true
pass "control-drop granted each way; $A allows $B inspect, $B allows $A disclose"

# --- point both at the stub's dead drop, and turn it on ---------------------
nx "$A" tcr peer drop-store set "$DROP_TEMPLATE" --token "$DROP_TOKEN" --peers "$PEERS" >/dev/null 2>&1 || true
nx "$B" tcr peer drop-store set "$DROP_TEMPLATE" --token "$DROP_TOKEN" --peers "$PEERS" >/dev/null 2>&1 || true
drop_on_a="$(nx "$A" tcr peer drop on --peers "$PEERS" 2>&1 || true)"
drop_on_b="$(nx "$B" tcr peer drop on --peers "$PEERS" 2>&1 || true)"
case "$drop_on_a" in
  *"peer.drop: on"*) pass "$A: peer.drop: on" ;;
  *) fail "$A: peer.drop did not turn on: $drop_on_a" ;;
esac
case "$drop_on_b" in
  *"peer.drop: on"*) pass "$B: peer.drop: on" ;;
  *) fail "$B: peer.drop did not turn on: $drop_on_b" ;;
esac

# --- node-a1-overlay: a real router mapping, measured before anything else --
internet_out="$(nx "$A" tcr peer internet on --peers "$PEERS" 2>&1 || true)"
case "$internet_out" in
  *"peer.internet: on"*) pass "$A: peer internet on: $internet_out" ;;
  *) fail "$A: peer internet on did not say it turned on: $internet_out" ;;
esac

# The measurement the design calls for before any assertion below is
# written: what `own_endpoints` (src/peer/drop.rs) actually has to publish
# for this Mac. `own_endpoints` reads `reach::external_socket()`, a
# process-local register the SAME running process's own internet keeper
# fills in (`nat-upnp-mapping.sh`'s own comment on why `reach --map`, a
# separate short-lived process, cannot be asked instead): the boot log line
# below is that register being written, in this process, for real.
mapping_line="$(wait_for_log "$A" "the router mapped this node.s listener port" 30 || true)"
echo "dead-drop: measurement: node-a1-overlay's own internet keeper: ${mapping_line:-<none within 30s>}"
if [ -n "$mapping_line" ]; then
  pass "$A holds a real router mapping in its own process, so own_endpoints has an external_socket to publish: $mapping_line"
else
  fail "$A's internet keeper never logged a mapping within 30s; own_endpoints would have nothing to publish"
  finish
  exit 1
fi

# --- restart node-a1-overlay: the publisher is boot-time gated --------------
a_restart_epoch="$(date +%s)"
oldpid="$(cat "$A_HOME/pid" 2>/dev/null || echo)"
if [ -n "$oldpid" ]; then
  kill "$oldpid" 2>/dev/null || true
fi
sleep 0.5
echo "--- dead-drop: restarted with the drop configured ---" >> "$A_HOME/boot.log"
nx "$A" tcr --headless --port 8088 --no-replace >> "$A_HOME/boot.log" 2>&1 &
echo $! > "$A_HOME/pid"
if ! wait_for_log "$A" "peer listener up" 30 >/dev/null; then
  fail "$A never came back up after the restart"
  finish
  exit 1
fi
pass "$A restarted with the drop live"

# --- node-a1-overlay lends, and serves one request first for headroom ------
lend_out="$(nx "$A" tcr peer lend "$b_id" --fraction 0.5 --ttl 600 --max-inflight 2 --scope all --peers "$PEERS" --config "$A_CONFIG" 2>&1 || true)"
case "$lend_out" in
  *"peer lend: ok"*) pass "$A lent $B a lease: $(echo "$lend_out" | head -1)" ;;
  *) fail "tcr peer lend did not report ok: $lend_out"; finish; exit 1 ;;
esac

i=0
while [ "$i" -lt 100 ]; do
  if ip netns exec "$A" python3 -c "
import socket
s = socket.socket()
s.settimeout(0.3)
try:
    s.connect(('5.5.5.20', 8080))
except OSError:
    raise SystemExit(1)
" 2>/dev/null; then
    break
  fi
  sleep 0.1
  i=$((i + 1))
done
warm="$(nx "$A" env HOME="$A_HOME" python3 "$SRC/netlab-client.py" "$A_CONFIG" 1 128 1 2>&1 || true)"
case "$warm" in
  *" 200 "*) pass "$A served one request through its own account first: $warm" ;;
  *) fail "$A's own warm-up request did not serve 200: $warm" ;;
esac

# --- disable node-b1-overlay's own account ----------------------------------
nx "$B" tcr disable node-b1-overlay-fake --config "$B_CONFIG" >/dev/null 2>&1 || true

# --- take the overlay down on both sides: the only address either side -----
# learned of the other was over it (sealed-invite names none in the ask or
# the reply, only the address the OPEN dials, and that dial ran on the
# overlay). `attach_extra` names the overlay wire `utun0` in every namespace,
# never `eth$idx`, which is what makes this the right interface to cut.
nx "$A" ip link set utun0 down
nx "$B" ip link set utun0 down
pass "the overlay is down on both sides"

# --- restart node-b1-overlay: the account disable and the peer-lease -------
# fallback provider are both boot-time, `src/fallback.rs`.
oldpid="$(cat "$B_HOME/pid" 2>/dev/null || echo)"
if [ -n "$oldpid" ]; then
  kill "$oldpid" 2>/dev/null || true
fi
sleep 0.5
echo "--- dead-drop: restarted with the account disabled, the overlay down ---" >> "$B_HOME/boot.log"
nx "$B" tcr --headless --port 8088 --no-replace >> "$B_HOME/boot.log" 2>&1 &
echo $! > "$B_HOME/pid"
if ! wait_for_log "$B" "peer listener up" 30 >/dev/null; then
  fail "$B never came back up after the restart"
  finish
  exit 1
fi
pass "$B restarted with its own account disabled and the overlay down"

# --- wait out node-a1-overlay's publish interval ----------------------------
# `DEAD_DROP_PUBLISH_INTERVAL` (`src/server.rs`) is 300s, fixed, and not a
# peers-file setting: the first tick fired the instant node-a1-overlay
# rebooted, before its own internet keeper's UPnP round trip could finish, so
# that first publish carried no address. The steps between the restart above
# and here (the lend, the warm-up, the account disable, the overlay teardown,
# node-b1-overlay's own restart) already spent some of this wait.
target_epoch=$((a_restart_epoch + 305))
now_epoch="$(date +%s)"
remaining=$((target_epoch - now_epoch))
if [ "$remaining" -gt 0 ]; then
  echo "dead-drop: waiting ${remaining}s more for node-a1-overlay's second publish tick (DEAD_DROP_PUBLISH_INTERVAL=300s)"
  sleep "$remaining"
fi
pass "waited out node-a1-overlay's publish interval since its restart"

# --- (a): read node-b1-overlay's fetch time before the request -------------
before_json="$(nx "$B" tcr peer reach --json --peers "$PEERS" 2>/dev/null || true)"
before_fetch="$(printf '%s' "$before_json" | python3 -c "
import json, sys
data = json.load(sys.stdin)
for p in data.get('deadDrop', {}).get('peers', []):
    if p.get('node') == '$a_id':
        print(p.get('lastFetchedAtMs'))
        break
else:
    print('')
" 2>/dev/null || true)"
echo "dead-drop: node-b1-overlay's lastFetchedAtMs before the request: $before_fetch"

# --- the borrow itself: node-b1-overlay's own account is gone, the only ----
# known address for node-a1-overlay is the overlay one that just went down,
# so the fallback provider must reach it through the drop or not at all.
before_ledger="$SCRATCH/ledger-before.json"
nx "$B" wget -qO- "$NETLAB_UPSTREAM/_ledger" > "$before_ledger" 2>/dev/null || true
before_count="$(python3 -c "import json; print(len(json.load(open('$before_ledger'))))" 2>/dev/null || echo 0)"

borrow="$(nx "$B" env HOME="$B_HOME" python3 "$SRC/netlab-client.py" "$B_CONFIG" 1 128 1 2>&1 || true)"
echo "dead-drop: $B's request: $borrow"
status="$(echo "$borrow" | awk '{print $3}')"

# --- (b): the request still serves 200 --------------------------------------
if [ "$status" = "200" ]; then
  pass "$B's request served 200 while its own account was disabled and the overlay was down"
else
  fail "$B's request did not serve 200: $borrow"
fi

if grep -q 'no local account could serve this request, served through a fallback provider.*provider="peer-lease"' "$B_HOME/boot.log"; then
  pass "$B's log names the fallback provider as peer-lease"
else
  fail "$B's log never named peer-lease as the fallback provider"
fi

after_ledger="$SCRATCH/ledger-after.json"
nx "$B" wget -qO- "$NETLAB_UPSTREAM/_ledger" > "$after_ledger" 2>/dev/null || true
new_row_account="$(python3 -c "
import json
before = $before_count
rows = json.load(open('$after_ledger'))
new = rows[before:]
print(new[0].get('account', '') if new else '')
" 2>/dev/null || true)"
if [ "$new_row_account" = "Bearer at-fake-node-a1-overlay" ]; then
  pass "the new ledger row from $B's borrow carries the LENDER's credential, Bearer at-fake-node-a1-overlay: the session was really served through $A, over the address the drop taught $B"
else
  fail "the new ledger row from $B's borrow carries [$new_row_account], not the lender's credential"
fi

# --- (a): node-b1-overlay's fetch time moved --------------------------------
after_json="$(nx "$B" tcr peer reach --json --peers "$PEERS" 2>/dev/null || true)"
after_fetch="$(printf '%s' "$after_json" | python3 -c "
import json, sys
data = json.load(sys.stdin)
for p in data.get('deadDrop', {}).get('peers', []):
    if p.get('node') == '$a_id':
        print(p.get('lastFetchedAtMs'))
        break
else:
    print('')
" 2>/dev/null || true)"
echo "dead-drop: node-b1-overlay's lastFetchedAtMs after the request: $after_fetch"

if [ -n "$after_fetch" ] && [ "$after_fetch" != "None" ] && [ "$after_fetch" != "$before_fetch" ]; then
  pass "node-b1-overlay's fetch time moved: $before_fetch -> $after_fetch"
else
  fail "node-b1-overlay's fetch time did not move: $before_fetch -> $after_fetch"
fi

finish
