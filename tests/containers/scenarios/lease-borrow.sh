#!/bin/sh
# lease-borrow
# One Mac lends, the other borrows, and a real request is served.
#
# Cast: node-a1 (lender, one working fake account), node-a2 (borrower, its own
# account disabled), upstream.
#
# Steps
#   1. pair the two
#   2. on node-a1: tcr peer lend <node-a2-id>, then the two grants a lease
#      needs on top of it: node-a1 allows node-a2 `inspect` (accept its
#      requests and serve them here) and node-a2 allows node-a1 `disclose`
#      (send this Mac's requests to that peer to serve). Neither is optional:
#      the borrowing side refused with `InspectNotGranted` without the first,
#      the lending side with the same refusal without the second (both read
#      the SAME grant name from the two ends of one relationship).
#   3. node-a1 serves one request through its OWN proxy first: `lendable`
#      (`Manager::lendable_fraction`) is measured headroom, and a lender that
#      has never served a request has none measured, which reads as nothing
#      to lend (`LeaseRefusal::OwnerGuard`), not a lab quirk, a real "an
#      account with zero observed traffic has no fraction to offer" rule.
#   4. disable node-a2's own account, restart it (the peer-lease provider
#      installs at boot, from the peers file; `src/fallback.rs`)
#   5. from node-a2's own namespace (loopback-bound proxy, `src/server.rs`):
#      POST /v1/messages
#
# Assertions
#   - node-a2's log carries `provider="peer-lease"` on the line "no local
#     account could serve this request, served through a fallback provider"
#     (src/proxy.rs, src/peer/lease.rs)
#   - the new row the ledger (`GET /_ledger` on the stub,
#     tests/containers/upstream-stub.py) gains from this request carries the
#     LENDER's credential, not the borrower's, a field, never a substring of
#     a log line. Not correlated by request_id: node-a1's own outbound call is
#     made over the peer CONTROL channel, not a plain HTTP forward, so it
#     carries none of the lab's x-netlab-* test headers node-a2's client sent
#    , measured, not assumed; see the comment at the ledger read below.
#   - the response is 200
set -eu

HERE="$(cd "$(dirname "$0")" && pwd)"
SCENARIO="lease-borrow"
export SCENARIO
# shellcheck source=../lib/assert.sh
. "$HERE/../lib/assert.sh"

if [ "${NETLAB:-0}" != "1" ]; then
  not_wired "compose: the steps above are written, the driving is only under --netlab"
fi

TOPOLOGY="$HERE/../topologies/lease-borrow.json"
export TOPOLOGY
NETLAB_UPSTREAM="http://10.77.1.30:8080"
export NETLAB_UPSTREAM
# shellcheck source=../lib/netlab.sh
. "$HERE/../lib/netlab.sh"

up node-a1 node-a2 || { finish; exit 1; }
pass "node-a1 and node-a2 are up"

# --- pair: node-a1 mints, node-a2 joins, mode A (pin on the spot) ----------
minted="$SCRATCH/minted.txt"
nx node-a1 tcr peer invite --peers /lab/node-a1/.config/tcr-peers.json --label node-a2 > "$minted" 2>&1 || true
token="$(grep -m1 '^tcr-join:' "$minted" || true)"
if [ -z "$token" ]; then
  fail "node-a1 minted no invite key: $(cat "$minted")"
  finish
  exit 1
fi
joined="$(printf '%s\n' "$token" | nxi node-a2 tcr peer join --stdin --peers /lab/node-a2/.config/tcr-peers.json --label node-a2 2>&1 || true)"
case "$joined" in
  *"peer join: ok"*) pass "node-a2 joined node-a1: $joined" ;;
  *) fail "node-a2's join did not report ok: $joined"; finish; exit 1 ;;
esac

a1_ls="$SCRATCH/a1-ls.json"
a2_ls="$SCRATCH/a2-ls.json"
ls_json node-a1 "$a1_ls" || true
ls_json node-a2 "$a2_ls" || true
a2_id="$(pinned_wire_id "$a1_ls" || true)"
a1_id="$(pinned_wire_id "$a2_ls" || true)"
if [ -z "$a2_id" ] || [ -z "$a1_id" ]; then
  fail "the pin did not resolve to exactly one id on each side (a1 sees:$a2_id a2 sees:$a1_id)"
  finish
  exit 1
fi
pass "node-a1 sees node-a2 as $a2_id, node-a2 sees node-a1 as $a1_id"

# --- lend, and the two grants a lease needs on top of it -------------------
lend_out="$(nx node-a1 tcr peer lend "$a2_id" --fraction 0.5 --ttl 600 --max-inflight 2 --scope all --peers /lab/node-a1/.config/tcr-peers.json --config /lab/node-a1/.config/teamclaude.json 2>&1 || true)"
case "$lend_out" in
  *"peer lend: ok"*) pass "node-a1 lent node-a2 a lease: $(echo "$lend_out" | head -1)" ;;
  *) fail "tcr peer lend did not report ok: $lend_out"; finish; exit 1 ;;
esac
nx node-a1 tcr peer allow "$a2_id" inspect on --peers /lab/node-a1/.config/tcr-peers.json >/dev/null 2>&1 || true
nx node-a2 tcr peer allow "$a1_id" disclose on --peers /lab/node-a2/.config/tcr-peers.json >/dev/null 2>&1 || true
pass "node-a1 allows node-a2 inspect, node-a2 allows node-a1 disclose"

# The upstream stub can still be settling under load right after `netlab up`
# (the same race `wait_reachable` in `netlab` narrows for a topology's own
# `clients`, measured in the netlab-client scenario): a real cross-namespace
# TCP probe here before node-a1's warm-up request, rather than trusting one
# retry budget inside netlab-client.py to always cover it.
i=0
while [ "$i" -lt 100 ]; do
  if ip netns exec node-a1 python3 -c "
import socket
s = socket.socket()
s.settimeout(0.3)
try:
    s.connect(('10.77.1.30', 8080))
except OSError:
    raise SystemExit(1)
" 2>/dev/null; then
    break
  fi
  sleep 0.1
  i=$((i + 1))
done

# node-a1 serves one request through its own proxy first, so it has measured
# headroom to lend: see the step-3 comment above.
warm="$(nx node-a1 env HOME=/lab/node-a1 python3 "$SRC/netlab-client.py" /lab/node-a1/.config/teamclaude.json 1 128 1 2>&1 || true)"
case "$warm" in
  *" 200 "*) pass "node-a1 served one request through its own account first: $warm" ;;
  *) fail "node-a1's own warm-up request did not serve 200: $warm" ;;
esac

# --- disable node-a2's own account, restart it ------------------------------
nx node-a2 tcr disable node-a2-fake --config /lab/node-a2/.config/teamclaude.json >/dev/null 2>&1 || true
oldpid="$(cat /lab/node-a2/pid 2>/dev/null || echo)"
if [ -n "$oldpid" ]; then
  kill "$oldpid" 2>/dev/null || true
fi
sleep 0.5
echo "--- lease-borrow: restarted with the account disabled ---" >> /lab/node-a2/boot.log
nx node-a2 tcr --headless --port 8089 --no-replace >> /lab/node-a2/boot.log 2>&1 &
echo $! > /lab/node-a2/pid
if ! wait_for_log node-a2 "peer listener up" 30 >/dev/null; then
  fail "node-a2 never came back up after the restart"
  finish
  exit 1
fi
pass "node-a2 restarted with its own account disabled"

# --- the borrow itself -------------------------------------------------------
# The ledger BEFORE: the request this relay makes is node-a1's own outbound
# call, over the peer CONTROL channel rather than a plain HTTP forward, so it
# carries none of the lab's x-netlab-* test headers node-a2's client sent , 
# there is no `request_id` to correlate on the other side of a relay like
# that, only the row COUNT and the credential the new row carries.
before="$SCRATCH/ledger-before.json"
nx node-a2 wget -qO- "$NETLAB_UPSTREAM/_ledger" > "$before" 2>/dev/null || true
before_count="$(python3 -c "import json; print(len(json.load(open('$before'))))" 2>/dev/null || echo 0)"

borrow="$(nx node-a2 env HOME=/lab/node-a2 python3 "$SRC/netlab-client.py" /lab/node-a2/.config/teamclaude.json 1 128 1 2>&1 || true)"
echo "lease-borrow: node-a2's request: $borrow"
status="$(echo "$borrow" | awk '{print $3}')"

if [ "$status" = "200" ]; then
  pass "node-a2's request served 200 while its own account was disabled"
else
  fail "node-a2's request did not serve 200: $borrow"
fi

if grep -q 'no local account could serve this request, served through a fallback provider.*provider="peer-lease"' /lab/node-a2/boot.log; then
  pass "node-a2's log names the fallback provider as peer-lease"
else
  fail "node-a2's log never named peer-lease as the fallback provider"
fi

after="$SCRATCH/ledger-after.json"
nx node-a2 wget -qO- "$NETLAB_UPSTREAM/_ledger" > "$after" 2>/dev/null || true
new_row_account="$(python3 -c "
import json
before = $before_count
rows = json.load(open('$after'))
new = rows[before:]
print(new[0].get('account', '') if new else '')
" 2>/dev/null || true)"
if [ "$new_row_account" = "Bearer at-fake-node-a1" ]; then
  pass "the new ledger row from node-a2's borrow carries the LENDER's credential, Bearer at-fake-node-a1"
else
  fail "the new ledger row from node-a2's borrow carries [$new_row_account], not the lender's credential"
fi

finish
