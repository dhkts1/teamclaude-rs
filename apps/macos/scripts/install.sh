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
# HOW THE INSTALL IS ORDERED, and that ordering is the point.
#
# The obvious `rm -rf "$DEST" && ditto "$SRC" "$DEST"` has two faults. For the
# whole duration of the copy /Applications/TcrBar.app does not exist — Spotlight,
# Launchpad, the login item and anyone double-clicking see nothing — and if the
# copy fails you are left with no app at all and no way back. So instead:
#
#   stage → verify → stop the app → swap → drop the old copy
#
# The staged bundle is ditto'd to a temp directory inside /Applications (same
# filesystem, which rename(2) requires) and code-signature-checked THERE. Only a
# bundle that passed the check is ever moved over the good one; a failed build or
# a broken signature aborts with the existing install untouched and still running.
# Because the check now happens before the swap it is load-bearing, which the
# old post-install `codesign -dv` printout never was.
#
# The swap is `mv` of directories, not a copy: rename(2) is atomic and the new
# bundle arrives with fresh inodes. That keeps the property `rm -rf` + `ditto`
# had and an in-place `cp -R` does not — nothing rewrites an executable another
# process is exec'ing, which is what SIGKILLed running binaries with
# `Code Signature Invalid` on 2026-08-06 (see scripts/install-cli.sh, same rule
# applied to a single file). A directory rename cannot clobber a directory, so
# the old bundle is moved aside first; if the second rename fails it is moved
# back. The only gap is between those two renames, and it is recoverable.
#
# Everything staged is removed by an EXIT trap on every CATCHABLE path, success
# or failure, so a failed install does not leave an orphan bundle sitting in
# /Applications. "Catchable" is the honest word and the limit is real: INT, TERM
# and HUP are trapped and turned into ordinary exits precisely so cleanup runs
# (see the traps below), but SIGKILL and a power loss cannot be caught by any
# shell, and either one between the two renames leaves a `.TcrBar.install.*`
# directory behind. Removing it by hand is the recovery.
#
# Idempotent: safe to re-run. Uninstall with scripts/uninstall.sh.
set -euo pipefail

MACOS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
APP_NAME="TcrBar"
SRC="$MACOS_DIR/build/${APP_NAME}.app"

# TCRBAR_DEV_BUILD=1 installs BESIDE the shipping app instead of over it.
#
# `build-tcrbar.sh` already honours this variable, but only for the bundle id
# (`io.github.dhkts1.tcrbar.dev`), which is what keeps a dev build from being
# the second process in the ControlCenter status-item race. The install path was
# still hardcoded, so a dev build was given its own identity and then written
# straight over /Applications/TcrBar.app anyway — the one thing the flag exists
# to prevent. Testing a local build therefore cost you the installed one, which
# is how a working release install was replaced by a build that could not
# self-update.
#
# Two bundles may share CFBundleName: LaunchServices, the login item and the
# status-item registration all key on the bundle id, and those already differ.
# The on-disk name differs so a human can tell them apart in /Applications.
if [ "${TCRBAR_DEV_BUILD:-0}" = "1" ]; then
  DEST="/Applications/${APP_NAME} Dev.app"
else
  DEST="/Applications/${APP_NAME}.app"
fi

# Match the process by its DESTINATION path, not by APP_NAME.
#
# The pattern has to distinguish the two installs, and "TcrBar.app/Contents/..."
# matches only the shipping one -- so on the dev path the running dev app would
# not be stopped before its bundle was swapped, and on the shipping path a
# running dev app is correctly left alone. Deriving it from $DEST gets both
# right, and keeps the original property that `pkill -f TcrBar` would not have:
# an editor with the name in its window title is never matched.
RUNNING_PATTERN="$DEST/Contents/MacOS/${APP_NAME}"

echo "==> Building ${APP_NAME}…"
bash "$MACOS_DIR/scripts/build-tcrbar.sh"

[ -d "$SRC" ] || { echo "build produced no bundle at $SRC" >&2; exit 1; }

# Staging directory lives beside the destination: rename(2) is only atomic within
# one filesystem, and /tmp is frequently a different one.
STAGE_DIR="$(mktemp -d "$(dirname "$DEST")/.${APP_NAME}.install.XXXXXX")"
STAGE="$STAGE_DIR/${APP_NAME}.app"
BACKUP="$STAGE_DIR/${APP_NAME}.previous.app"

# Runs on every exit path. If we died between the two renames the destination is
# missing and the old bundle is still in the staging directory — put it back
# before the staging directory is removed, or the cleanup would delete the only
# copy of the app.
#
# It also has to speak up about the app it stopped. Once the running TcrBar is
# pkill'd, any later failure restores the OLD bundle to disk but leaves nothing
# running — and the script used to exit silently, so the user was left with a
# vanished menu-bar item and no statement that it had been deliberately stopped.
# Relaunching automatically would be wrong (a failed install may have left the
# reason for the failure in place), so say it plainly instead.
STOPPED_APP=0
cleanup() {
  rc=$?
  if [ ! -e "$DEST" ] && [ -d "$BACKUP" ]; then
    mv "$BACKUP" "$DEST" || true
  fi
  rm -rf "$STAGE_DIR"
  if [ "$rc" -ne 0 ] && [ "$STOPPED_APP" = "1" ]; then
    echo "" >&2
    echo "NOTE: ${APP_NAME} was stopped before this failed, and was NOT restarted." >&2
    if [ -e "$DEST" ]; then
      echo "      The previous install is still at $DEST — relaunch it with:" >&2
      echo "        open \"$DEST\"" >&2
    else
      echo "      Nothing is installed at $DEST — re-run this script once the build is fixed." >&2
    fi
  fi
}
trap cleanup EXIT
# A shell killed by an UNCAUGHT signal does not run its EXIT trap — verified in
# the sandbox gate, where a SIGTERM mid-ditto orphaned the staging directory.
# Catching the signal turns it into an ordinary exit, which does run cleanup.
trap 'exit 130' INT
trap 'exit 143' TERM HUP

echo "==> Staging the new bundle…"
# ditto, not cp -R: it preserves the bundle's resource forks, extended
# attributes and the code signature. A cp -R can invalidate the signature.
ditto "$SRC" "$STAGE"

echo "==> Verifying the staged bundle…"
# --deep --strict, because the inner Contents/MacOS/tcr is signed before the
# bundle around it and only a deep check proves that order held (apps/macos/README.md).
# This runs while the currently installed app is still in place: a failure here
# aborts and leaves it exactly as it was.
if ! codesign -v --deep --strict "$STAGE"; then
  echo "staged bundle at $SRC failed code-signature verification — refusing to install" >&2
  echo "the existing install at $DEST is untouched" >&2
  exit 1
fi

# Stop only OUR app, and only the copy being replaced. `pkill -f TcrBar` would
# also match an editor with the name in its title or a grep for it. This happens
# after verification so a bad build never costs you the running app.
if pgrep -f "$RUNNING_PATTERN" >/dev/null 2>&1; then
  echo "==> Stopping the running $(basename "$DEST" .app)…"
  pkill -f "$RUNNING_PATTERN" || true
  STOPPED_APP=1
  # Give the supervised child, if any, a moment to be reaped cleanly.
  sleep 1
fi

echo "==> Installing to ${DEST}…"
if [ -e "$DEST" ]; then
  mv "$DEST" "$BACKUP"
fi
if ! mv "$STAGE" "$DEST"; then
  echo "could not move the staged bundle into place — restoring the previous install" >&2
  exit 1
fi
rm -rf "$BACKUP"

# Re-register so LaunchServices knows about the new copy immediately rather than
# whenever it next rescans.
/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister \
  -f "$DEST" >/dev/null 2>&1 || true

# Informational only — the gate that can still save you already ran, above, on
# the staged copy. This just shows what landed.
echo "==> Installed signature…"
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
