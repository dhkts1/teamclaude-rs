#!/bin/sh
# moved-link
# Mint a moved link, open it, and open it again.
#
# Cast: node-a1-overlay (10.77.1.11 on home-a, 100.64.99.11 on overlay) and
# node-a2 (10.77.1.12 on home-a), paired both ways over home-a and then greeted
# once.
#
# **The two nodes are NOT on the same set of networks, and that is the point.**
# A link says "here is where I am now", so a scenario where the sender holds
# nothing the receiver does not already know cannot tell a link that carries the
# right addresses from one that carries junk: both read `already-known`. Pairing
# happens over home-a, so node-a2 learns 10.77.1.11:7755 and only that; the
# overlay address is the one fact the link has to carry, and every assertion
# below about what was added names it.
#
# node-a2 is not on the overlay network and cannot dial 100.64.99.11. It does not
# have to: what is measured here is what a link teaches a peers file, which is a
# row fact, and the dial itself is `nat-no-mapping`'s and `overlay-invite`'s.
#
# The greeting is not decoration. A link is sealed under the pair's rendezvous
# secret, and that secret is derived from a completed handshake: a freshly
# paired row carries none, and `mint` on one refuses for want of a key rather
# than sealing under something guessable. One hello gives both sides their copy,
# because both ends derive it from the same handshake.
#
# Steps
#   1. up node-a1-overlay node-a2, pair each way over home-a, node-a2 greets
#   2. on node-a1-overlay: tcr peer moved mint <node-a2 id>
#   3. on node-a2: tcr peer moved open --stdin, the preview
#   4. on node-a2: tcr peer moved open --stdin --yes, the same link
#   5. on node-a2: tcr peer moved open --stdin --yes, the same link again
#
# Assertions
#   - the link carries both of the sender's addresses, not one of them
#   - the preview names the overlay address it would add, and says nothing was
#     written
#   - the first --yes says how many addresses it added
#   - node-a2's row afterwards holds the address it was paired at AND the
#     overlay address the link taught it
#   - the second --yes prints `already-known`
#   - the peers file's mtime is unchanged by that second open
#
# The count is fixed at twenty: every assertion here is one named fact, and
# none of them loops over a row's endpoints. A scenario whose total moves with
# the data cannot be read as a number, and the endpoint-per-line shape belongs
# to `hello-no-wildcard-endpoint`, which is about what a row must NOT hold.
#
# The link goes in on standard input and never in an argument, which is the
# path the panel uses and the one that does not put a secret in `ps`.
set -eu

HERE="$(cd "$(dirname "$0")" && pwd)"
SCENARIO="moved-link"
export SCENARIO
# shellcheck source=../lib/assert.sh
. "$HERE/../lib/assert.sh"
# shellcheck source=../lib/compose.sh
. "$HERE/../lib/compose.sh"

# The compose services this scenario needs, for the runner and for a reader.
SERVICES="node-a1-overlay node-a2"
export SERVICES

# The three addresses this scenario names, written once. The first two are what
# the two Macs are paired at; the third is the one only the link can teach.
A1_HOME=10.77.1.11
A2_HOME=10.77.1.12
A1_OVERLAY=100.64.99.11

# shellcheck disable=SC2086 # SERVICES is a deliberate list of service names
up $SERVICES || { finish; exit 1; }

pair_nodes node-a2 node-a1-overlay "$A1_HOME:$LISTEN_PORT" || { finish; exit 1; }
pair_nodes node-a1-overlay node-a2 "$A2_HOME:$LISTEN_PORT" || { finish; exit 1; }

# Each node's wire id, read off the file of the Mac that pinned it. That is the
# only place it exists in the form `tcr peer moved mint` takes: `tcr peer id`
# prints the short display form, which `PeerId::parse` refuses on purpose.
a1_ls="$SCRATCH/a1-pinned.json"
a2_ls="$SCRATCH/a2-pinned.json"
ls_json node-a1-overlay "$a1_ls" || true
ls_json node-a2 "$a2_ls" || true
a2_id="$(pinned_wire_id "$a1_ls" || true)"
a1_id="$(pinned_wire_id "$a2_ls" || true)"
if [ -z "$a1_id" ] || [ -z "$a2_id" ]; then
  fail "a node does not pin exactly one Mac after two pairings (a1=$a1_id a2=$a2_id)"
  finish
  exit 1
fi

greeted="$(nx node-a2 tcr peer hello "$a1_id" --peers "$PEERS" 2>&1 || true)"
case "$greeted" in
  *"peer hello: ok"*) pass "node-a2 greeted node-a1, so both rows hold this pair's shared secret" ;;
  *)
    fail "node-a2 greeting node-a1: $greeted"
    finish
    exit 1
    ;;
esac

# --- mint ----------------------------------------------------------------
minted="$SCRATCH/minted.txt"
nx node-a1-overlay tcr peer moved mint "$a2_id" --peers "$PEERS" > "$minted" 2>&1 || true
link="$(grep -m1 '^tcr://peer/moved' "$minted" || true)"
if [ -z "$link" ]; then
  fail "node-a1 minted no link for $a2_id: $(cat "$minted")"
  finish
  exit 1
fi
pass "node-a1 minted one link sealed for node-a2: $(grep -m1 'sealed for' "$minted" || echo "$link" | cut -c1-24)..."

# Both of this Mac's addresses, not one. The positive control for everything
# below: a link carrying a single address could still be the right one by luck,
# and a link carrying the wildcard bind address would count as one too.
carried="$(grep -m1 'it carries' "$minted" || true)"
case "$carried" in
  *"carries 2 address(es)"*) pass "the link carries both of node-a1's addresses: $carried" ;;
  *) fail "the link does not carry this Mac's two addresses: $carried" ;;
esac

# --- open: the preview ---------------------------------------------------
preview="$(printf '%s\n' "$link" | nxi node-a2 tcr peer moved open --stdin --peers "$PEERS" 2>&1 || true)"
echo "$preview" | sed 's/^/moved-link: preview: /'
case "$preview" in
  *"peer moved: would add $A1_OVERLAY:$LISTEN_PORT"*)
    pass "the preview names the overlay address it would add: $A1_OVERLAY:$LISTEN_PORT" ;;
  *) fail "the preview did not name the one address node-a2 does not already hold: $preview" ;;
esac
case "$preview" in
  *"nothing written; pass --yes"*) pass "the preview wrote nothing and said so" ;;
  *) fail "the preview did not say it wrote nothing: $preview" ;;
esac

# --- open: the write -----------------------------------------------------
wrote="$(printf '%s\n' "$link" | nxi node-a2 tcr peer moved open --stdin --yes --peers "$PEERS" 2>&1 || true)"
echo "$wrote" | sed 's/^/moved-link: first --yes: /'
case "$wrote" in
  *"peer moved: added "*) pass "the first --yes added addresses: $(echo "$wrote" | grep -m1 'peer moved: added ')" ;;
  *) fail "the first --yes added nothing: $wrote" ;;
esac

# --- the row the write left behind ---------------------------------------
# Both facts, because either alone is satisfied by a bug: a row holding only the
# overlay address means the link overwrote what the pairing proved, and a row
# holding only the paired address means the write did not land.
after_ls="$SCRATCH/a2-after.json"
after_endpoints="$SCRATCH/a2-after-endpoints.txt"
if ls_json node-a2 "$after_ls" && python3 "$LIB/peer-read.py" endpoints "$a1_id" < "$after_ls" > "$after_endpoints" 2>&1; then
  if grep -q "^direct $A1_HOME:$LISTEN_PORT$" "$after_endpoints"; then
    pass "node-a2 still holds the address it paired at, $A1_HOME:$LISTEN_PORT"
  else
    fail "the link cost node-a2 the address it paired at: $(cat "$after_endpoints")"
  fi
  if grep -q "^direct $A1_OVERLAY:$LISTEN_PORT$" "$after_endpoints"; then
    pass "node-a2 now also holds the overlay address the link taught it, $A1_OVERLAY:$LISTEN_PORT"
  else
    fail "node-a2's row learned no overlay address: $(cat "$after_endpoints")"
  fi
else
  fail "node-a2: could not read its own row for node-a1 after the write: $(cat "$after_endpoints" 2>/dev/null)"
  fail "and so the two facts about that row were never checked"
fi

# --- open: the same link again -------------------------------------------
# The mtime is read before and after, in seconds since the epoch, off the
# container's own `stat`: a second paste of one link is meant to cost nothing,
# and "nothing" means the operator's file is not rewritten to move a timestamp.
before="$(nx node-a2 stat -c %Y "$PEERS" 2>/dev/null | tr -d '\r\n' || true)"
again="$(printf '%s\n' "$link" | nxi node-a2 tcr peer moved open --stdin --yes --peers "$PEERS" 2>&1 || true)"
after="$(nx node-a2 stat -c %Y "$PEERS" 2>/dev/null | tr -d '\r\n' || true)"
echo "$again" | sed 's/^/moved-link: second --yes: /'
case "$again" in
  *"peer moved: already-known"*) pass "the same link a second time reads already-known" ;;
  *) fail "the same link a second time did not read already-known: $again" ;;
esac
if [ -z "$before" ] || [ -z "$after" ]; then
  fail "could not read the peers file's mtime either side of the second open (before=$before after=$after)"
elif [ "$before" = "$after" ]; then
  pass "the second open left the peers file's mtime at $after"
else
  fail "the second open rewrote the peers file: mtime $before became $after"
fi

finish
