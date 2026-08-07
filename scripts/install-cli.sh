#!/usr/bin/env bash
# Install the release `tcr` binary onto PATH.
#
# Usage: scripts/install-cli.sh [--force] [destination]    (default: ~/.local/bin/tcr)
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
#
#   3. That same rename(2) property has a second, unwanted consequence, so the
#      script refuses to run when the destination is a symlink. apps/macos/README.md
#      documents the intended arrangement where ~/.local/bin/tcr is a SYMLINK into
#      /Applications/TcrBar.app/Contents/MacOS/tcr, so the menu-bar app and the CLI
#      are one artifact and only one thing ever needs updating. Because `mv -f`
#      replaces a symlink instead of following it, running this installer over that
#      link turns it back into an independent regular file -- silently, with no
#      error -- and binary drift between the app and the CLI is back. Refusing is
#      the only way that undoing is visible; TCR_INSTALL_FORCE=1 (or --force) is
#      the deliberate override.
set -euo pipefail

FORCE="${TCR_INSTALL_FORCE:-0}"
DEST=""
for arg in "$@"; do
  case "$arg" in
    --force) FORCE=1 ;;
    *)       DEST="$arg" ;;
  esac
done
DEST="${DEST:-$HOME/.local/bin/tcr}"

fail() { printf '!! %s\n' "$1" >&2; exit 1; }

command -v cargo >/dev/null 2>&1 || fail "cargo not found on PATH"
command -v jq    >/dev/null 2>&1 || fail "jq not found on PATH"

TARGET_DIR="$(cargo metadata --format-version 1 --no-deps | jq -r .target_directory)"
[ -n "$TARGET_DIR" ] && [ "$TARGET_DIR" != "null" ] || fail "could not resolve cargo target directory"
SRC="$TARGET_DIR/release/tcr"

[ -f "$SRC" ] || fail "no release binary at $SRC -- build it first:  cargo build --release"
[ -x "$SRC" ] || fail "$SRC is not executable"

# See note 3 in the header: `mv -f` replaces a symlink destination rather than
# following it, so installing over the app-bundle symlink would quietly split the
# one shared binary back into two that drift apart.
if [ -L "$DEST" ] && [ "$FORCE" != "1" ]; then
  fail "$(printf '%s\n' \
    "symlinked destination -- refusing to install" \
    "  $DEST -> $(readlink "$DEST")" \
    "It is a symlink, most likely the shared TcrBar.app binary documented in" \
    "apps/macos/README.md. Installing here would replace the link with a regular" \
    "file and reintroduce drift between the app and the CLI." \
    "Instead:" \
    "  update the bundle:  apps/macos/scripts/install.sh" \
    "  or install elsewhere:  scripts/install-cli.sh /some/other/path/tcr" \
    "  or override on purpose:  scripts/install-cli.sh --force  (TCR_INSTALL_FORCE=1)")"
fi

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
