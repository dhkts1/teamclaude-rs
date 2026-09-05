#!/usr/bin/env bash
# install-tcrbar-from-dmg.sh — mount a TcrBar dmg and swap it into /Applications.
#
# Shared by install.sh (the one-line installer, step 2) and `tcr ui` (which
# `include_str!`s this file rather than re-implementing the mount/swap in
# Rust) so there is exactly one copy of this logic, not two that drift.
#
# Usage: install-tcrbar-from-dmg.sh <path-to-dmg>
#
# Mirrors apps/macos/scripts/install.sh's staging discipline for the same
# reasons documented there: ditto (not cp -R) preserves the code signature,
# the staging dir lives beside the destination so the final move is an atomic
# rename(2), and an EXIT trap cleans up on every catchable path so a failed
# install never leaves an orphan bundle or a mounted dmg behind. This script
# does not build anything and does not stop a running app itself — the
# caller (install.sh) already refused before getting here if TcrBar is
# running, per CLAUDE.md: you cannot replace /Applications/TcrBar.app while
# it is running, because its bundled `tcr` is an executing image inside the
# very bundle being swapped.
set -euo pipefail

if [ "$#" -ne 1 ]; then
  echo "usage: $0 <path-to-dmg>" >&2
  exit 1
fi

DMG="$1"
APP_NAME="TcrBar"
DEST="/Applications/${APP_NAME}.app"

if [ ! -f "$DMG" ]; then
  echo "no dmg at $DMG" >&2
  exit 1
fi

MOUNT_DIR=""
STAGE_DIR=""

cleanup() {
  rc=$?
  if [ -n "$STAGE_DIR" ]; then
    rm -rf "$STAGE_DIR"
  fi
  if [ -n "$MOUNT_DIR" ] && [ -d "$MOUNT_DIR" ]; then
    hdiutil detach "$MOUNT_DIR" -quiet -force >/dev/null 2>&1 || true
  fi
  exit "$rc"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM HUP

MOUNT_DIR="$(mktemp -d)"
echo "==> Mounting $DMG…"
hdiutil attach -nobrowse -readonly -mountpoint "$MOUNT_DIR" "$DMG" >/dev/null

SRC="$MOUNT_DIR/${APP_NAME}.app"
if [ ! -d "$SRC" ]; then
  echo "no ${APP_NAME}.app found inside $DMG" >&2
  exit 1
fi

# Staging directory lives beside the destination: rename(2) is only atomic
# within one filesystem, and /tmp is frequently a different one.
STAGE_DIR="$(mktemp -d "$(dirname "$DEST")/.${APP_NAME}.install.XXXXXX")"
STAGE="$STAGE_DIR/${APP_NAME}.app"
BACKUP="$STAGE_DIR/${APP_NAME}.previous.app"

echo "==> Staging the new bundle…"
ditto "$SRC" "$STAGE"

echo "==> Verifying the staged bundle…"
if ! codesign -v --deep --strict "$STAGE"; then
  echo "staged bundle from $DMG failed code-signature verification — refusing to install" >&2
  exit 1
fi

echo "==> Installing to ${DEST}…"
if [ -e "$DEST" ]; then
  mv "$DEST" "$BACKUP"
fi
if ! mv "$STAGE" "$DEST"; then
  echo "could not move the staged bundle into place — restoring the previous install" >&2
  if [ -d "$BACKUP" ]; then
    mv "$BACKUP" "$DEST" || true
  fi
  exit 1
fi
rm -rf "$BACKUP"

/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister \
  -f "$DEST" >/dev/null 2>&1 || true

echo "==> Installed: $DEST"
