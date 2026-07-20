#!/usr/bin/env bash
#
# validate-cache.sh — live prompt-cache warmth validator for tcr (teamclaude-rs)
# =============================================================================
#
# WHAT THIS PROVES
#   Fires TWO POST /v1/messages turns at a LIVE tcr proxy, both carrying the
#   SAME metadata.user_id (tcr's session-affinity key — see stable_session_key
#   in src/proxy.rs) and an IDENTICAL large system prompt marked with a
#   cache_control:{"type":"ephemeral"} breakpoint.
#
#     turn 1 (cold)  -> Anthropic CREATES the cache: cache_creation_input_tokens > 0,
#                       cache_read_input_tokens == 0
#     turn 2 (warm)  -> Anthropic READS the cache:   cache_read_input_tokens  > 0   <-- PASS condition
#
#   A PASS is end-to-end runtime proof that (a) tcr pinned both turns of the
#   session to the SAME upstream account (otherwise the cache — which lives on
#   the account's Anthropic side — would be cold on turn 2), and (b) the prompt
#   cache was genuinely warm. This is the one thing a fake/stub upstream cannot
#   fake: only real Anthropic can return a non-zero cache_read count. tcr's own
#   surfaces cannot show this — the cache-read count is summed into a quota
#   total and discarded — so we read it out-of-band from the response bodies.
#
# THE LIVE RUN IS THE OPERATOR'S STEP.
#   This script talks to a real tcr + spends real Anthropic quota. It CANNOT be
#   run to a verdict in a sandbox. Use `--dry-run` to inspect the exact request
#   bodies and assertions offline; run it for real only against Gil's live tcr.
#
# USAGE
#   scripts/validate-cache.sh              # live run against $TCR_URL
#   scripts/validate-cache.sh --dry-run    # print request bodies + assertions, send nothing
#   scripts/validate-cache.sh --model claude-sonnet-4-5
#   TCR_URL=http://127.0.0.1:3456 scripts/validate-cache.sh
#
# SECURITY
#   No secrets, tokens, or account names live in this file. It targets
#   localhost:3456, and tcr injects the real upstream auth itself.
# =============================================================================

set -euo pipefail

# ---- configuration (edit here) ---------------------------------------------
MODEL="${MODEL:-claude-sonnet-4-5}"          # override with --model or MODEL=...
TCR_URL="${TCR_URL:-http://127.0.0.1:3456}"
ANTHROPIC_VERSION="2023-06-01"
MAX_TOKENS=64                                # keep the completions tiny/cheap

DRY_RUN=0

# ---- arg parsing ------------------------------------------------------------
while [[ $# -gt 0 ]]; do
  case "$1" in
    --dry-run) DRY_RUN=1; shift ;;
    --model)   MODEL="${2:?--model needs a value}"; shift 2 ;;
    --model=*) MODEL="${1#*=}"; shift ;;
    -h|--help)
      sed -n '2,45p' "$0"
      exit 0
      ;;
    *) echo "unknown argument: $1 (try --help)" >&2; exit 2 ;;
  esac
done

# ---- dependency check -------------------------------------------------------
for dep in curl jq; do
  if ! command -v "$dep" >/dev/null 2>&1; then
    echo "FATAL: required dependency '$dep' not found on PATH" >&2
    exit 3
  fi
done

# ---- session identity -------------------------------------------------------
# A single stable user_id for BOTH turns is the whole point: it is what pins the
# session onto one account (stable_session_key hashes "uid:<user_id>"). Unique
# per run so the log cross-check below isolates THIS run's two turns.
RUN_ID="cache-validate-$(date +%Y%m%d-%H%M%S)-$$"
USER_ID="$RUN_ID"

# ---- the cached prefix ------------------------------------------------------
# A large, IDENTICAL system prompt on both turns, ending in a cache_control
# ephemeral breakpoint. It must be big enough to clear Anthropic's minimum
# cacheable prefix (~1024 tokens for sonnet); we repeat a filler block to be
# safely over that threshold. The text content is irrelevant — only that it is
# byte-identical across the two turns and marked cacheable.
FILLER_UNIT="You are a meticulous assistant participating in a prompt-cache warmth validation. \
This paragraph is deliberately verbose filler whose only job is to push the cached system \
prefix comfortably past the minimum cacheable-token threshold so that the ephemeral \
cache_control breakpoint below actually creates a reusable cache entry on the first turn. "
SYSTEM_TEXT=""
for _ in $(seq 1 40); do
  SYSTEM_TEXT+="$FILLER_UNIT"
done

# Build the shared system block (array with one cache_control breakpoint) via jq
# so quoting/escaping is correct regardless of the filler content.
SYSTEM_JSON="$(jq -n --arg t "$SYSTEM_TEXT" \
  '[{"type":"text","text":$t,"cache_control":{"type":"ephemeral"}}]')"

# ---- request-body builder ---------------------------------------------------
# $1 = user message text for this turn. The system prefix + metadata.user_id are
# identical across turns; only the user message differs.
build_body() {
  local user_text="$1"
  jq -n \
    --arg model "$MODEL" \
    --argjson max_tokens "$MAX_TOKENS" \
    --argjson system "$SYSTEM_JSON" \
    --arg uid "$USER_ID" \
    --arg user_text "$user_text" \
    '{
      model: $model,
      max_tokens: $max_tokens,
      stream: false,
      system: $system,
      metadata: { user_id: $uid },
      messages: [ { role: "user", content: $user_text } ]
    }'
}

TURN1_TEXT="Turn 1: reply with the single word OK."
TURN2_TEXT="Turn 2: reply with the single word DONE."
BODY1="$(build_body "$TURN1_TEXT")"
BODY2="$(build_body "$TURN2_TEXT")"

LOG_HINT="grep 'serving request' \"\${TMPDIR:-/tmp}/teamclaude-rs.log\" | tail"

# ---- dry-run path -----------------------------------------------------------
if [[ "$DRY_RUN" -eq 1 ]]; then
  echo "=== DRY RUN — nothing will be sent ==="
  echo "TCR_URL              : $TCR_URL"
  echo "endpoint             : $TCR_URL/v1/messages"
  echo "model                : $MODEL"
  echo "anthropic-version    : $ANTHROPIC_VERSION"
  echo "metadata.user_id     : $USER_ID   (SAME on both turns -> one account)"
  echo "system prefix bytes  : ${#SYSTEM_TEXT}   (identical + cache_control ephemeral on both turns)"
  echo
  echo "--- turn 1 request body (cold: expect cache_creation>0, cache_read=0) ---"
  echo "$BODY1" | jq .
  echo
  echo "--- turn 2 request body (warm: expect cache_read>0  <-- PASS condition) ---"
  echo "$BODY2" | jq .
  echo
  echo "--- assertions ---"
  echo "turn 1 : .usage.cache_creation_input_tokens > 0   AND  .usage.cache_read_input_tokens == 0"
  echo "turn 2 : .usage.cache_read_input_tokens > 0        (this alone determines PASS/FAIL)"
  echo
  echo "--- affinity cross-check (operator runs after a live run) ---"
  echo "$LOG_HINT"
  echo "  -> the two most-recent lines for this run must name the SAME session=<id>"
  echo "     AND the SAME account=<name>. Same session + same account = the pin held."
  echo "     'serving request' (snapshot.rs:28) carries session+account and logs before"
  echo "     the upstream outcome; 'upstream response' (proxy.rs:419) carries account+status"
  echo "     but no session, so it cannot distinguish concurrent sessions — we use the former."
  echo
  echo "=== END DRY RUN — no requests sent, no quota spent ==="
  exit 0
fi

# ---- live POST helper -------------------------------------------------------
# Sends one turn, echoes the raw JSON response on stdout. Fails loudly on a
# transport error or a non-JSON / error-shaped body.
post_turn() {
  local body="$1" label="$2" resp
  resp="$(curl -sS -X POST "$TCR_URL/v1/messages" \
    -H 'content-type: application/json' \
    -H "anthropic-version: $ANTHROPIC_VERSION" \
    --data "$body")" || {
      echo "FATAL: curl failed sending $label to $TCR_URL/v1/messages" >&2
      exit 4
    }
  if ! echo "$resp" | jq -e . >/dev/null 2>&1; then
    echo "FATAL: $label response was not valid JSON:" >&2
    echo "$resp" >&2
    exit 5
  fi
  if [[ "$(echo "$resp" | jq -r '.type // empty')" == "error" ]]; then
    echo "FATAL: $label returned an API error:" >&2
    echo "$resp" | jq . >&2
    exit 6
  fi
  echo "$resp"
}

# usage extractor: prints "creation read input" (nulls -> 0) for a response
usage_triplet() {
  echo "$1" | jq -r '.usage | "\(.cache_creation_input_tokens // 0) \(.cache_read_input_tokens // 0) \(.input_tokens // 0)"'
}

echo "=== live prompt-cache validation against $TCR_URL (user_id=$USER_ID) ==="

# ---- turn 1 (cold) ----------------------------------------------------------
RESP1="$(post_turn "$BODY1" "turn 1")"
read -r CREATION1 READ1 INPUT1 <<<"$(usage_triplet "$RESP1")"
echo "turn=1 cache_creation=$CREATION1 cache_read=$READ1 input=$INPUT1"

# Brief settle so tcr's affinity + Anthropic's cache write are visible to turn 2.
sleep 2

# ---- turn 2 (warm) ----------------------------------------------------------
RESP2="$(post_turn "$BODY2" "turn 2")"
read -r CREATION2 READ2 INPUT2 <<<"$(usage_triplet "$RESP2")"
echo "turn=2 cache_creation=$CREATION2 cache_read=$READ2 input=$INPUT2"

# ---- verdict ----------------------------------------------------------------
echo
echo "full usage blocks:"
echo "  turn 1:" ; echo "$RESP1" | jq '.usage'
echo "  turn 2:" ; echo "$RESP2" | jq '.usage'
echo

WARM="no"
if [[ "$READ2" =~ ^[0-9]+$ && "$READ2" -gt 0 ]]; then
  WARM="yes"
fi

echo "affinity cross-check (confirm the two most-recent lines share the SAME session= AND account=):"
echo "  $LOG_HINT"
echo

if [[ "$WARM" == "yes" ]]; then
  echo "RESULT: PASS — cache warm on turn 2: yes (cache_read=$READ2)"
  exit 0
else
  echo "RESULT: FAIL — cache warm on turn 2: no (cache_read=$READ2)"
  echo "  Likely causes: session affinity did not pin both turns to one account," \
       "the prefix was under the cacheable minimum, or the two turns hit different accounts." >&2
  exit 1
fi
