#!/usr/bin/env bash
# Rename the pre-split `throttle` key to `accountThrottle` + `fleetThrottle`.
#
# AS OF THE NEXT RELEASE, THE BINARY DOES THIS FOR YOU. `config::load` now
# migrates a stale `throttle` key automatically (in memory always, and to disk
# too on the server's boot path, once) instead of rejecting it — an
# auto-updated install self-heals on its own next start, with no manual step.
# This script has no callers in this repo any more. It stays useful for
# exactly one case: pre-migrating a config before installing an OLDER binary
# that still hard-rejects `throttle`, i.e. the reverse direction from normal
# upgrades. Whether to delete it outright is a separate call, not made here.
#
# ORDER MATTERS. Run this BEFORE installing the new binary, never after.
#
#   1. scripts/migrate-throttle-config.sh          <- you are here
#   2. scripts/install-cli.sh
#   3. restart the proxy (at a quiet moment)
#
# Why that order and not the reverse: the new binary REJECTS a config carrying
# the old `throttle` key, but `tcr server` does not fail closed on a config
# error. `main.rs::load_config` warns on stderr and falls back to a config with
# ZERO accounts, so a proxy started against a stale config looks alive and
# serves nothing. Migrating first avoids that window entirely, and is safe
# because the OLD binary tolerates the new keys: `Config` has a
# `#[serde(flatten)] extra` catch-all, so it ignores them and falls back to its
# own `default_throttle()`. Verified 2026-08-27 against the 034880a binary.
#
# Safe to run against the live config: jq rewrites the whole document, so every
# other key (including credentials) keeps its value. The original is backed up
# first, and the script refuses to write if it would touch any key beyond the
# ones it names.
#
# Usage:
#   scripts/migrate-throttle-config.sh [--dry-run] [--enable-noise-exempt] [path]
#
#   --enable-noise-exempt   ALSO set `throttleExemptNoise: true`, letting
#                           telemetry skip the per-org bucket. This is a real
#                           behaviour change and is OFF unless you ask for it.
set -euo pipefail

dry_run=0
enable_noise_exempt=0
config=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --dry-run)             dry_run=1; shift ;;
    --enable-noise-exempt) enable_noise_exempt=1; shift ;;
    -h|--help)             sed -n '2,30p' "$0"; exit 0 ;;
    -*)                    echo "unknown flag: $1" >&2; exit 2 ;;
    *)                     config="$1"; shift ;;
  esac
done

config="${config:-$HOME/.config/teamclaude.json}"

if [[ ! -f "$config" ]]; then
  echo "no config at $config" >&2
  exit 1
fi

if ! jq -e . "$config" >/dev/null 2>&1; then
  echo "config at $config is not valid JSON — refusing to touch it" >&2
  exit 1
fi

if ! jq -e 'has("throttle")' "$config" >/dev/null; then
  echo "no legacy \`throttle\` key in $config — nothing to migrate."
  echo "current: accountThrottle=$(jq -c '.accountThrottle // "absent (defaults ON)"' "$config")" \
       "fleetThrottle=$(jq -c '.fleetThrottle // "absent (defaults ON)"' "$config")"
  exit 0
fi

# REFUSE the escape-hatch form. `"throttle": {}` meant "no throttling at all",
# and no single new key means that — disabling one bucket leaves the other at its
# default. Any mapping this script picked would silently change what the operator
# wrote. This is the same reasoning that makes the new binary reject the key at
# load rather than migrate it; a migration script that quietly does the migration
# the loader refuses to do would defeat the point.
if [[ "$(jq -c '.throttle' "$config")" == "{}" ]]; then
  cat >&2 <<'MSG'
REFUSING: this config has `"throttle": {}` — the pre-split escape hatch meaning
"no throttling at all". There is no single key that still means that, so any
automatic mapping would change your intent without telling you.

Decide explicitly and edit by hand:

  fully unthrottled (what {} used to mean):
      "accountThrottle": {}, "fleetThrottle": {}

  adopt the new defaults:
      delete the `throttle` key and let both default to ON

MSG
  exit 1
fi

old_spacing=$(jq -r '.throttle.minSpacingMs // "unset"' "$config")
old_burst=$(jq -r '.throttle.burst // "unset"' "$config")

# The old `burst` is NOT carried across, and that is deliberate rather than
# sloppy: it was a FLEET-WIDE budget and the new one is PER-ORGANIZATION. The two
# are different quantities that happen to share a name, so copying the number
# would look like preservation while silently meaning something else. Both new
# keys get the shipped defaults; the old values are printed so the operator can
# see exactly what changed and override afterwards if they had tuned it.
migrated=$(jq '
    del(.throttle)
  | .accountThrottle = { minSpacingMs: 350, burst: 8 }
  | .fleetThrottle   = { minSpacingMs: 100, burst: 16 }
' "$config")

if [[ "$enable_noise_exempt" == 1 ]]; then
  migrated=$(jq '.throttleExemptNoise = true' <<<"$migrated")
fi

echo "--- was ---"
echo "  throttle: minSpacingMs=$old_spacing burst=$old_burst   (fleet-wide)"
echo "--- now ---"
jq -n --argjson m "$migrated" '$m | {accountThrottle, fleetThrottle}'
if [[ "$enable_noise_exempt" == 1 ]]; then
  echo "  throttleExemptNoise: true   (telemetry skips the per-org bucket)"
else
  echo "  throttleExemptNoise: untouched"
fi
if [[ "$old_burst" != "unset" && "$old_burst" != "8" ]]; then
  echo ""
  echo "NOTE: your old burst was $old_burst, fleet-wide. It is NOT carried over —"
  echo "      the new burst is per-organization, a different quantity. Edit"
  echo "      accountThrottle.burst by hand if you want something other than 8."
fi

if [[ "$dry_run" == 1 ]]; then
  echo "--- dry run: $config NOT modified ---"
  exit 0
fi

# The gate is built from what this run INTENDED to change, so an unintended edit
# is caught rather than pre-approved. `throttleExemptNoise` is only permitted
# when it was explicitly requested.
allowed='["accountThrottle","fleetThrottle","throttle"]'
if [[ "$enable_noise_exempt" == 1 ]]; then
  allowed='["accountThrottle","fleetThrottle","throttle","throttleExemptNoise"]'
fi

changed=$(jq -n --slurpfile a "$config" --argjson b "$migrated" '
  ($a[0] | keys_unsorted) as $ak
  | ($b     | keys_unsorted) as $bk
  | (($ak + $bk) | unique)
  | map(select(($a[0][.] // null) != ($b[.] // null)))
')
if ! jq -n --argjson c "$changed" --argjson ok "$allowed" -e '$c - $ok == []' >/dev/null; then
  echo "REFUSING TO WRITE: migration would also change $(jq -nc --argjson c "$changed" --argjson ok "$allowed" '$c - $ok')" >&2
  exit 1
fi

backup="${config}.pre-throttle-split.$(date +%Y%m%d-%H%M%S)"
cp -p "$config" "$backup"
printf '%s\n' "$migrated" > "$config"
echo ""
echo "migrated. backup at $backup"
echo "NEXT: install the new binary (scripts/install-cli.sh), THEN restart."
echo "      The running proxy is unaffected until it restarts — the old binary"
echo "      ignores these keys and keeps its current 350ms/burst-4 behaviour."
