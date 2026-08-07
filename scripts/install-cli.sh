#!/usr/bin/env bash
# Install the release `tcr` binary onto PATH.
#
# Usage: scripts/install-cli.sh [destination]        (default: ~/.local/bin/tcr)
#
# Two things here are deliberate, and both exist because of a specific incident.
#
#   1. The source is resolved from `cargo metadata`, never from the literal
#      `target/release`. With CARGO_TARGET_DIR exported -- as it is on this
#      machine -- cargo writes nowhere near the repo, so `<repo>/target/release/tcr`
#      is a stale orphan from before the variable was set. Installing it silently
#      ships old code and every "the fix is live" claim afterwards is false.
#
#   2. The binary is copied to a temp file in the DESTINATION'S OWN directory and
#      then renamed over the destination -- never `cp` straight onto it. A `cp`
#      onto a live path is an in-place, same-inode rewrite of an executable other
#      processes are exec'ing; macOS answers by SIGKILLing them with
#      `Code Signature Invalid` / `Taskgated Invalid Signature`. That is not
#      hypothetical: 25 such crash reports landed in ~/Library/Logs/DiagnosticReports/
#      between 19:56:55 and 19:57:22 on 2026-08-06, from exactly this pattern.
#      rename(2) is atomic and gives the destination a NEW inode, so already-running
#      execs keep their old inode intact. It also replaces a *symlink* destination
#      rather than following it -- `cp` would write through the link and corrupt
#      whatever it pointed at, which is how the 2026-08-06 burst reached the repo's
#      own build output.
set -euo pipefail

DEST="${1:-$HOME/.local/bin/tcr}"

fail() { printf '!! %s\n' "$1" >&2; exit 1; }

command -v cargo >/dev/null 2>&1 || fail "cargo not found on PATH"
command -v jq    >/dev/null 2>&1 || fail "jq not found on PATH"

TARGET_DIR="$(cargo metadata --format-version 1 --no-deps | jq -r .target_directory)"
[ -n "$TARGET_DIR" ] && [ "$TARGET_DIR" != "null" ] || fail "could not resolve cargo target directory"
SRC="$TARGET_DIR/release/tcr"

[ -f "$SRC" ] || fail "no release binary at $SRC -- build it first:  cargo build --release"
[ -x "$SRC" ] || fail "$SRC is not executable"

DEST_DIR="$(dirname "$DEST")"
mkdir -p "$DEST_DIR"

# Temp file must live in the destination's directory: rename(2) is only atomic
# within a single filesystem, and /tmp is frequently a different one.
TMP="$(mktemp "$DEST_DIR/.tcr.install.XXXXXX")"
trap 'rm -f "$TMP"' EXIT

cp "$SRC" "$TMP"
chmod 755 "$TMP"

if command -v codesign >/dev/null 2>&1; then
  codesign -v "$TMP" 2>/dev/null || fail "codesign verification failed for $SRC -- refusing to install"
fi

mv -f "$TMP" "$DEST"
trap - EXIT

sha() { shasum -a 256 "$1" 2>/dev/null | awk '{print $1}' || sha256sum "$1" | awk '{print $1}'; }

echo "source:    $SRC"
echo "           $(sha "$SRC")"
echo "installed: $DEST"
echo "           $(sha "$DEST")"
