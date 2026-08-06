#!/usr/bin/env bash
# install.sh — build TcrBar and install it to /Applications.
#
# WHY THIS EXISTS, and it is not tidiness.
#
# `swift build` leaves the bundle in apps/macos/build/, which is gitignored and
# replaced wholesale on every rebuild. Two things break if you run it from there:
#
#  1. Launch at login. `SMAppService.mainApp` registers the bundle at the path it
#     currently occupies. Rebuild, clean, or move the checkout and macOS is left
#     holding a login item aimed at a bundle that is no longer there.
#  2. LaunchServices. Replacing a running bundle in place is what produces
#     `_LSOpenURLsWithCompletionHandler() failed with error -600` — the app was
#     swapped underneath the launcher.
#
# /Applications is a stable path, so the login item survives rebuilds and the app
# shows up in Spotlight and Launchpad like anything else.
#
# Idempotent: safe to re-run. Uninstall with scripts/uninstall.sh.
set -euo pipefail

MACOS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
APP_NAME="TcrBar"
SRC="$MACOS_DIR/build/${APP_NAME}.app"
DEST="/Applications/${APP_NAME}.app"

echo "==> Building ${APP_NAME}…"
bash "$MACOS_DIR/scripts/build-tcrbar.sh"

[ -d "$SRC" ] || { echo "build produced no bundle at $SRC" >&2; exit 1; }

# Stop only OUR app, and only the copy being replaced. `pkill -f TcrBar` would
# also match an editor with the name in its title or a grep for it.
if pgrep -f "${APP_NAME}.app/Contents/MacOS/${APP_NAME}" >/dev/null 2>&1; then
  echo "==> Stopping the running ${APP_NAME}…"
  pkill -f "${APP_NAME}.app/Contents/MacOS/${APP_NAME}" || true
  # Give the supervised child, if any, a moment to be reaped cleanly.
  sleep 1
fi

echo "==> Installing to ${DEST}…"
# ditto, not cp -R: it preserves the bundle's resource forks, extended
# attributes and the code signature. A cp -R can invalidate the signature.
rm -rf "$DEST"
ditto "$SRC" "$DEST"

# Re-register so LaunchServices knows about the new copy immediately rather than
# whenever it next rescans.
/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister \
  -f "$DEST" >/dev/null 2>&1 || true

echo "==> Verifying the installed bundle…"
codesign -dv "$DEST" 2>&1 | rg -i 'Identifier|Authority=Apple|Signature' | sed 's/^/    /' || true

echo "==> Launching…"
open "$DEST"

cat <<INSTRUCTIONS

Installed: $DEST

  * The menu-bar item is the whole app — there is no Dock icon (LSUIElement).
  * "Launch at login" now registers a STABLE path, so it survives rebuilds.
  * "Start server at launch" brings the proxy up with --no-replace, so it can
    never disturb a proxy that is already serving.

  NOTE: once TcrBar supervises the server, quitting the app STOPS that server.
  That is deliberate — it only ever terminates a child it spawned itself — but it
  does mean Quit is an expensive action while supervising.

  Re-run this script after any rebuild. Remove with scripts/uninstall.sh.
INSTRUCTIONS
