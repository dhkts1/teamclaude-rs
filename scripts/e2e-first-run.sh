#!/usr/bin/env bash
# e2e-first-run.sh — what a brand-new machine sees on its first `tcr` commands.
#
# Runs a freshly built `tcr` (argument 1) against an EMPTY, throwaway HOME and
# asserts the three first-run behaviours that shipped in #308 and #309:
#
#   1. no config on disk        -> the verb exits 0 and creates the config
#   2. a Claude Code login file -> the server boot imports it as the first account
#   3. a second run             -> imports nothing (idempotent), status reads it
#
# Written for the CI runners' stock shells (macOS ships bash 3.2), so no bash 4+
# features; JSON is read with python3, present on every GitHub runner. Never run
# this against a real HOME: it writes a fake credential file and starts a proxy.
set -euo pipefail

TCR="${1:?usage: e2e-first-run.sh <path-to-tcr>}"
TCR="$(cd "$(dirname "$TCR")" && pwd)/$(basename "$TCR")"

E2E_HOME="$(mktemp -d)"
export HOME="$E2E_HOME"
unset XDG_CACHE_HOME XDG_CONFIG_HOME TCR_CLAUDE_CODE_CREDENTIALS || true
CONFIG="$HOME/.config/teamclaude.json"
# On CI the config does not exist before the server boots, which is the real
# first run. On a developer Mac a live proxy usually holds :3456, so
# TCR_E2E_PORT=<free port> pre-seeds `{"proxy":{"port":N}}` for cases 2 and 3 —
# still zero accounts, still the import trigger, just not the default port.
# `tcr status` has no --port flag; it reads the port from the config.
E2E_PORT="${TCR_E2E_PORT:-}"
SERVER_PID=""

cleanup() {
  if [ -n "$SERVER_PID" ] && kill -0 "$SERVER_PID" 2>/dev/null; then
    kill "$SERVER_PID" 2>/dev/null || true
    wait "$SERVER_PID" 2>/dev/null || true
  fi
  rm -rf "$E2E_HOME"
}
trap cleanup EXIT

fail() { echo "FAIL: $*" >&2; exit 1; }
pass() { echo "PASS: $*"; }

accounts_count() {
  python3 -c 'import json,sys; print(len(json.load(open(sys.argv[1])).get("accounts", [])))' "$CONFIG"
}

# --- 1. no config: every verb exits 0 and the file appears -------------------
[ ! -e "$CONFIG" ] || fail "temp HOME already has a config"
"$TCR" accounts >/tmp/e2e-accounts-1.out 2>/tmp/e2e-accounts-1.err || fail "tcr accounts exited $? with no config: $(cat /tmp/e2e-accounts-1.err)"
[ -f "$CONFIG" ] || fail "tcr accounts did not create $CONFIG"
[ "$(accounts_count)" = "0" ] || fail "fresh config should have 0 accounts, has $(accounts_count)"
grep -q "no accounts configured" /tmp/e2e-accounts-1.err || fail "expected the zero-accounts hint on stderr, got: $(cat /tmp/e2e-accounts-1.err)"
# GNU stat first (Linux runners, and a Mac with coreutils on PATH), BSD second.
perms="$(stat -c '%a' "$CONFIG" 2>/dev/null || stat -f '%Lp' "$CONFIG")"
[ "$perms" = "600" ] || fail "config perms are $perms, want 600"
pass "no config: tcr accounts exit 0, created $CONFIG (0600, 0 accounts, hint printed)"

# --- 2. a Claude Code login file is imported at server boot ------------------
rm -f "$CONFIG"
if [ -n "$E2E_PORT" ]; then
  printf '{"proxy":{"port":%s}}\n' "$E2E_PORT" >"$CONFIG"
  chmod 600 "$CONFIG"
fi
mkdir -p "$HOME/.claude"
# Obviously fake values: the profile fetch will fail upstream, so the account
# is named by the fallback ("unnamed"). The point is the wiring, not the token.
cat >"$HOME/.claude/.credentials.json" <<'JSON'
{"claudeAiOauth":{"accessToken":"sk-ant-oat01-fake-e2e-access","refreshToken":"sk-ant-ort01-fake-e2e-refresh","expiresAt":4102444800000,"refreshTokenExpiresAt":4102444800000,"scopes":["user:inference","user:profile"],"subscriptionType":"max","rateLimitTier":"default_claude_max_5x"}}
JSON
chmod 600 "$HOME/.claude/.credentials.json"

"$TCR" --headless ${E2E_PORT:+--port "$E2E_PORT"} >/tmp/e2e-server.log 2>&1 &
SERVER_PID=$!
for _ in $(seq 1 60); do
  if "$TCR" status --json >/tmp/e2e-status-1.out 2>/tmp/e2e-status-1.err; then break; fi
  kill -0 "$SERVER_PID" 2>/dev/null || fail "server died during boot: $(cat /tmp/e2e-server.log)"
  sleep 0.5
done
[ -f "$CONFIG" ] || fail "server boot did not create $CONFIG"
[ -n "$E2E_PORT" ] || [ "$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["proxy"]["port"])' "$CONFIG")" = "3456" ] || fail "fresh config did not get the default port"
[ "$(accounts_count)" = "1" ] || fail "server boot should have imported 1 account, config has $(accounts_count); server log: $(cat /tmp/e2e-server.log)"
python3 - "$CONFIG" <<'PY' || fail "imported row is not the Claude Code login"
import json, sys
a = json.load(open(sys.argv[1]))["accounts"][0]
assert a.get("refreshToken") == "sk-ant-ort01-fake-e2e-refresh", a.keys()
assert a.get("expiresAt") == 4102444800000, a.get("expiresAt")
PY
grep -q "imported" /tmp/e2e-server.log || fail "server log has no import line: $(cat /tmp/e2e-server.log)"
pass "server boot imported the Claude Code login as the first account"

# --- 3. idempotent: a second verb imports nothing, status reads one account --
"$TCR" accounts >/tmp/e2e-accounts-2.out 2>/tmp/e2e-accounts-2.err || fail "second tcr accounts exited $?"
[ "$(accounts_count)" = "1" ] || fail "second run changed the account count to $(accounts_count)"
"$TCR" status --json >/tmp/e2e-status-2.out 2>/tmp/e2e-status-2.err || fail "tcr status --json exited $? against the running server: $(cat /tmp/e2e-status-2.err)"
python3 -c 'import json,sys; d=json.load(open(sys.argv[1])); rows=d if isinstance(d,list) else d.get("accounts",[]); assert len(rows)==1, len(rows)' /tmp/e2e-status-2.out || fail "status --json does not list exactly one account: $(head -c 400 /tmp/e2e-status-2.out)"
pass "second run imported nothing; tcr status --json lists the one account"

echo "e2e-first-run: all three first-run cases pass on $(uname -s)"
