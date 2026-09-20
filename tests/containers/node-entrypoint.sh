#!/bin/sh
# One node: write a scratch config and a scratch peers file, apply the opt-ins a
# scenario asked for, then become the server.
#
# The config is written here rather than mounted so that no file in this
# container ever comes from an operator's machine. Every value is obviously
# fake, and the upstream is the stub in the compose project.
#
# Env, all optional:
#   NODE_NAME      the display name this node announces and pairs under
#   PEER_LISTEN    what goes in the peers file's `listen` (default 0.0.0.0:7755)
#   PROXY_PORT     the local proxy port (default 8088)
#   UPSTREAM       the stub upstream base URL (default http://upstream:8080)
#   FIND           `on` to set the discovery flag before the server boots
#   ANNOUNCE_NAME  `on` to put the display name in the beacon
#   DEFAULT_VIA    address of this node's router, made the default route
#   BOOT_LOG       where the server's own log goes (default /scratch/boot.log)
set -eu

NODE_NAME="${NODE_NAME:-node}"
PEER_LISTEN="${PEER_LISTEN:-0.0.0.0:7755}"
PROXY_PORT="${PROXY_PORT:-8088}"
UPSTREAM="${UPSTREAM:-http://upstream:8080}"
FIND="${FIND:-off}"
ANNOUNCE_NAME="${ANNOUNCE_NAME:-off}"
BOOT_LOG="${BOOT_LOG:-/scratch/boot.log}"

CONFIG=/scratch/.config/teamclaude.json
PEERS=/scratch/.config/tcr-peers.json

mkdir -p /scratch/.config /scratch/.cache

# A home network is `internal: true`, so Docker gives this container no default
# route at all and the scenario names the router instead. That route is not a
# convenience: `reach.rs` reads /proc/net/route to find the gateway it sends the
# NAT-PMP probe to, so a node with no default route probes nothing and the NAT
# scenarios would pass by never asking.
if [ -n "${DEFAULT_VIA:-}" ]; then
  ip route replace default via "$DEFAULT_VIA"
  echo "node: default route via $DEFAULT_VIA"
fi

cat > "$CONFIG" <<JSON
{
  "proxy": { "port": ${PROXY_PORT} },
  "upstream": "${UPSTREAM}",
  "quotaProbeSeconds": 0,
  "warmupSeconds": 0,
  "accounts": [
    {
      "name": "${NODE_NAME}-fake",
      "accessToken": "at-fake-${NODE_NAME}",
      "accountUuid": "11111111-1111-1111-1111-111111111111",
      "orgUuid": "22222222-2222-2222-2222-222222222222"
    }
  ]
}
JSON

# `config::read_or_default` refuses a peers file it did not write unless the
# mode is 0600, and a proxy with no peer port gives no clue why.
printf '{"listen":"%s","peers":[]}\n' "$PEER_LISTEN" > "$PEERS"
chmod 600 "$PEERS"

tcr peer name "$NODE_NAME" --peers "$PEERS" >/dev/null
if [ "$FIND" = "on" ]; then
  # Before the server boots, so the beacon starts with it rather than within
  # the twenty seconds the running server takes to re-read the flag.
  tcr peer find on --peers "$PEERS" --announce-name "$ANNOUNCE_NAME" >/dev/null
fi

exec tcr --headless --port "$PROXY_PORT" --no-replace >>"$BOOT_LOG" 2>&1
