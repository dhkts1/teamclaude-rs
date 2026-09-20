#!/bin/sh
# moved-link
# Mint a moved link, open it, and open it again.
#
# Cast: node-a1 (10.77.1.11) and node-a2 (10.77.1.12) on home-a, paired both
# ways and then greeted once.
#
# The greeting is not decoration. A link is sealed under the pair's rendezvous
# secret, and that secret is derived from a completed handshake: a freshly
# paired row carries none, and `mint` on one refuses for want of a key rather
# than sealing under something guessable. One hello gives both sides their copy,
# because both ends derive it from the same handshake.
#
# Steps
#   1. up node-a1 node-a2, pair each way, node-a2 greets node-a1
#   2. on node-a1: tcr peer moved mint <node-a2 id>
#   3. on node-a2: tcr peer moved open --stdin, the preview
#   4. on node-a2: tcr peer moved open --stdin --yes, the same link
#   5. on node-a2: tcr peer moved open --stdin --yes, the same link again
#
# Assertions
#   - the preview names what it would add and says nothing was written
#   - the first --yes says how many addresses it added
#   - the second --yes prints `already-known`
#   - the peers file's mtime is unchanged by that second open
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
SERVICES="node-a1 node-a2"
export SERVICES

# shellcheck disable=SC2086 # SERVICES is a deliberate list of service names
up $SERVICES || { finish; exit 1; }

pair_nodes node-a2 node-a1 "10.77.1.11:$LISTEN_PORT" || { finish; exit 1; }
pair_nodes node-a1 node-a2 "10.77.1.12:$LISTEN_PORT" || { finish; exit 1; }

# Each node's wire id, read off the file of the Mac that pinned it. That is the
# only place it exists in the form `tcr peer moved mint` takes: `tcr peer id`
# prints the short display form, which `PeerId::parse` refuses on purpose.
a1_ls="$SCRATCH/a1-pinned.json"
a2_ls="$SCRATCH/a2-pinned.json"
ls_json node-a1 "$a1_ls" || true
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
nx node-a1 tcr peer moved mint "$a2_id" --peers "$PEERS" > "$minted" 2>&1 || true
link="$(grep -m1 '^tcr://peer/moved' "$minted" || true)"
if [ -z "$link" ]; then
  fail "node-a1 minted no link for $a2_id: $(cat "$minted")"
  finish
  exit 1
fi
pass "node-a1 minted one link sealed for node-a2: $(grep -m1 'sealed for' "$minted" || echo "$link" | cut -c1-24)..."

# --- open: the preview ---------------------------------------------------
preview="$(printf '%s\n' "$link" | nxi node-a2 tcr peer moved open --stdin --peers "$PEERS" 2>&1 || true)"
echo "$preview" | sed 's/^/moved-link: preview: /'
case "$preview" in
  *"peer moved: would add "*) pass "the preview names the address it would add" ;;
  *) fail "the preview named nothing it would add: $preview" ;;
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
