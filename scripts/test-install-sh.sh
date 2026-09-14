#!/usr/bin/env bash
# test-install-sh.sh — proves install.sh's TcrBar step survives a real
# `curl | sh` with no local checkout to fall back to.
#
# Runs the target install.sh through `sh` on stdin, from an empty directory —
# exactly the shape of the README's one-liner (`curl … | sh`), never
# `bash install.sh`, because BASH_SOURCE[0] resolving under a checkout is
# precisely the bug this test exists to catch. A local http server stands in
# for github.com/api.github.com/raw.githubusercontent.com so the run touches
# no real network and no real /Applications:
#   - a fake TcrBar.app packed into a real dmg with hdiutil (needs macOS)
#   - a fake releases/latest JSON
#   - a copy of scripts/install-tcrbar-from-dmg.sh, standing in for the raw
#     GitHub fetch install.sh does when it finds no local checkout
#
# Usage: scripts/test-install-sh.sh [path-to-install.sh]
#   Defaults to this repo's own install.sh. Pass a copy of an older
#   install.sh (e.g. `git show main:install.sh` written to a temp file) to
#   prove the test fails against the pre-fix script.
set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
INSTALL_SH="${1:-$REPO_ROOT/install.sh}"

if [ ! -f "$INSTALL_SH" ]; then
  echo "no install.sh at $INSTALL_SH" >&2
  exit 1
fi

if [ "$(uname -s)" != "Darwin" ]; then
  echo "this test needs hdiutil — macOS only" >&2
  exit 1
fi

WORKDIR="$(mktemp -d)"
SERVER_PID=""

# shellcheck disable=SC2329 # invoked indirectly via `trap cleanup EXIT` below
cleanup() {
  rc=$?
  if [ -n "$SERVER_PID" ]; then
    kill "$SERVER_PID" >/dev/null 2>&1 || true
    wait "$SERVER_PID" 2>/dev/null || true
  fi
  rm -rf "$WORKDIR"
  exit "$rc"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

APP_NAME="TcrBar"
VERSION="0.0.0-test"
TAG="v${VERSION}"

SERVER_ROOT="$WORKDIR/server"
FAKE_APP="$WORKDIR/fake-app/${APP_NAME}.app"
mkdir -p "$FAKE_APP/Contents/MacOS" "$SERVER_ROOT/releases/download/${TAG}"

cat > "$FAKE_APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict>
<key>CFBundleExecutable</key><string>${APP_NAME}</string>
<key>CFBundleIdentifier</key><string>com.example.${APP_NAME}</string>
<key>CFBundleShortVersionString</key><string>${VERSION}</string>
</dict></plist>
PLIST

cat > "$FAKE_APP/Contents/MacOS/${APP_NAME}" <<'BIN'
#!/bin/sh
exit 0
BIN
chmod +x "$FAKE_APP/Contents/MacOS/${APP_NAME}"

# install-tcrbar-from-dmg.sh's own codesign -v --deep --strict check refuses
# an unsigned bundle — ad-hoc sign the fake app so the test exercises that
# check as a real pass, not a skip.
codesign --force --deep --sign - "$FAKE_APP" >/dev/null 2>&1

DMG_PATH="$SERVER_ROOT/releases/download/${TAG}/${APP_NAME}-${VERSION}.dmg"
echo "==> building fake dmg at $DMG_PATH"
if ! hdiutil create -volname "$APP_NAME" -srcfolder "$FAKE_APP" -ov -format UDZO "$DMG_PATH" >/dev/null; then
  echo "hdiutil create failed" >&2
  exit 1
fi

cat > "$SERVER_ROOT/releases/latest" <<JSON
{"tag_name": "${TAG}"}
JSON

cp "$REPO_ROOT/scripts/install-tcrbar-from-dmg.sh" "$SERVER_ROOT/install-tcrbar-from-dmg.sh"

PORT=$(( (RANDOM % 20000) + 20000 ))
(
  cd "$SERVER_ROOT" || exit 1
  exec python3 -m http.server "$PORT" --bind 127.0.0.1 >/dev/null 2>&1
) &
SERVER_PID=$!

up=0
for _ in $(seq 1 50); do
  if curl -fsS "http://127.0.0.1:${PORT}/releases/latest" >/dev/null 2>&1; then
    up=1
    break
  fi
  sleep 0.1
done
if [ "$up" -ne 1 ]; then
  echo "local http server on :${PORT} never came up" >&2
  exit 1
fi

RUN_DIR="$WORKDIR/run"
APPLICATIONS_DIR="$WORKDIR/Applications"
mkdir -p "$RUN_DIR" "$APPLICATIONS_DIR"

# The candidate install.sh's own "already installed and running" guard reads
# the REAL /Applications and shells out to the REAL pgrep — on a dev machine
# that already has TcrBar running (see this repo's CLAUDE.md) that guard
# fires and the script exits before ever reaching the code path this test
# exists to exercise, regardless of which install.sh is under test. This test
# only cares whether TcrBar.app lands in the FAKE Applications dir below, so
# shadow pgrep with one that always reports "no match" — never touches or
# signals the real process, just answers the question install.sh asks.
FAKEBIN="$WORKDIR/fakebin"
mkdir -p "$FAKEBIN"
cat > "$FAKEBIN/pgrep" <<'EOF'
#!/bin/sh
exit 1
EOF
chmod +x "$FAKEBIN/pgrep"

export TCR_SKIP_CLI=1
export TCR_APPLICATIONS_DIR="$APPLICATIONS_DIR"
export TCR_LATEST_RELEASE_API_URL="http://127.0.0.1:${PORT}/releases/latest"
export TCR_DMG_URL_BASE="http://127.0.0.1:${PORT}/releases/download"
export TCR_DMG_INSTALL_SCRIPT_URL="http://127.0.0.1:${PORT}/install-tcrbar-from-dmg.sh"
export PATH="$FAKEBIN:$PATH"

echo "==> running install.sh piped through sh, from an empty directory ($RUN_DIR)…"
cd "$RUN_DIR" || exit 1
cat "$INSTALL_SH" | sh
install_rc=$?
echo "install.sh exit: $install_rc"

if [ -d "${APPLICATIONS_DIR}/${APP_NAME}.app" ]; then
  echo "PASS: ${APPLICATIONS_DIR}/${APP_NAME}.app exists"
  exit 0
else
  echo "FAIL: ${APPLICATIONS_DIR}/${APP_NAME}.app was not created" >&2
  exit 1
fi
