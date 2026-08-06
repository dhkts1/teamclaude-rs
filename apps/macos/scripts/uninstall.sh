#!/usr/bin/env bash
# uninstall.sh — remove TcrBar from /Applications, reversibly and completely.
#
# An install that cannot be undone is not an install, it is a mess. This also
# unregisters the login item: leaving it behind means macOS keeps trying to
# launch a bundle that no longer exists, which surfaces to the user as a vague
# login-items error with nothing to click.
set -euo pipefail

APP_NAME="TcrBar"
BUNDLE_ID="com.github.dhkts1.tcrbar"
DEST="/Applications/${APP_NAME}.app"

if pgrep -f "${APP_NAME}.app/Contents/MacOS/${APP_NAME}" >/dev/null 2>&1; then
  echo "==> Stopping ${APP_NAME}…"
  echo "    (any server it was supervising stops with it — that is by design)"
  pkill -f "${APP_NAME}.app/Contents/MacOS/${APP_NAME}" || true
  sleep 1
fi

# Unregister the login item before deleting the bundle: SMAppService keys on the
# bundle, so removing the app first can strand the registration.
if [ -d "$DEST" ]; then
  echo "==> Unregistering the login item…"
  /usr/bin/osascript -e "tell application \"System Events\" to delete login item \"${APP_NAME}\"" \
    >/dev/null 2>&1 || true
  /System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister \
    -u "$DEST" >/dev/null 2>&1 || true

  echo "==> Removing ${DEST}…"
  rm -rf "$DEST"
else
  echo "==> Nothing installed at ${DEST}."
fi

echo
echo "Removed. The app's preferences are untouched; clear them with:"
echo "    defaults delete ${BUNDLE_ID}"
echo
echo "Nothing about tcr itself was changed. If a server was running independently"
echo "of TcrBar, it is still running."
