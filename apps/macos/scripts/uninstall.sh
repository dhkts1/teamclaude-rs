#!/usr/bin/env bash
# uninstall.sh — remove TcrBar from /Applications, reversibly and completely.
#
# An install that cannot be undone is not an install, it is a mess. This also
# unregisters the login item: leaving it behind means macOS keeps trying to
# launch a bundle that no longer exists, which surfaces to the user as a vague
# login-items error with nothing to click.
set -euo pipefail

APP_NAME="TcrBar"
BUNDLE_ID="io.github.dhkts1.tcrbar"
# The id TcrBar shipped under until 2026-08-09, when ControlCenter's runtime
# blocked list — keyed on bundle id — swallowed it and forced a new identity.
# An uninstall that leaves the old prefs domain and the old LaunchServices
# registration behind is not an uninstall: `open -b` can still resolve a stale
# copy, and the abandoned domain keeps the hidden-status-item defaults alive
# across a reinstall.
LEGACY_BUNDLE_ID="com.github.dhkts1.tcrbar"
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

# Legacy identity cleanup. Unregister by PATH, because `lsregister` takes a
# bundle path and not an id — so ask Spotlight which bundles still claim the old
# id first. A machine that never ran the old build finds nothing and prints
# nothing, which is the intended no-op.
legacy_copies="$(/usr/bin/mdfind "kMDItemCFBundleIdentifier == '${LEGACY_BUNDLE_ID}'" 2>/dev/null || true)"
if [ -n "$legacy_copies" ]; then
  echo "==> Unregistering copies still claiming ${LEGACY_BUNDLE_ID}…"
  echo "$legacy_copies" | while IFS= read -r legacy_app; do
    [ -n "$legacy_app" ] || continue
    echo "    $legacy_app"
    /System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister \
      -u "$legacy_app" >/dev/null 2>&1 || true
  done
fi

# The old domain is unconditionally dead — no build will ever read it again —
# so unlike the current one it is deleted rather than merely reported. Leaving
# it behind is how a hidden-status-item default outlives a reinstall.
if /usr/bin/defaults read "${LEGACY_BUNDLE_ID}" >/dev/null 2>&1; then
  echo "==> Removing stale preferences for ${LEGACY_BUNDLE_ID}…"
  /usr/bin/defaults delete "${LEGACY_BUNDLE_ID}" >/dev/null 2>&1 || true
fi

echo
echo "Removed. The app's preferences are untouched; clear them with:"
echo "    defaults delete ${BUNDLE_ID}"
echo
echo "Nothing about tcr itself was changed. If a server was running independently"
echo "of TcrBar, it is still running."
