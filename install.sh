#!/usr/bin/env bash
# install.sh — the one-line installer: `tcr` CLI, then TcrBar on macOS.
#
# Step 1 is exactly the cargo-dist shell installer teamclaude-rs-installer.sh
# always was — regenerated every release from dist-workspace.toml, no
# post-install hook, not something this script edits. Step 2 is new: on
# macOS, install TcrBar too, so the default install gets both halves of the
# system instead of leaving the app as a manual dmg download.
#
# `set -e` is deliberately NOT used past the curl|sh pipe: that pipe's exit
# status is judged explicitly (PIPESTATUS[0] is the curl leg, PIPESTATUS[1]
# is `sh` running the installer) so a curl failure and an installer failure
# are told apart, and one bad step 2 does not retroactively make step 1 look
# like it failed.
set -uo pipefail

DIST_INSTALLER_URL="https://github.com/dhkts1/teamclaude-rs/releases/latest/download/teamclaude-rs-installer.sh"
LATEST_RELEASE_API_URL="${TCR_LATEST_RELEASE_API_URL:-https://api.github.com/repos/dhkts1/teamclaude-rs/releases/latest}"
DMG_URL_BASE="${TCR_DMG_URL_BASE:-https://github.com/dhkts1/teamclaude-rs/releases/download}"
DMG_INSTALL_SCRIPT_URL="${TCR_DMG_INSTALL_SCRIPT_URL:-https://raw.githubusercontent.com/dhkts1/teamclaude-rs/main/scripts/install-tcrbar-from-dmg.sh}"
APP_NAME="TcrBar"
APPLICATIONS_DIR="${TCR_APPLICATIONS_DIR:-/Applications}"

usage() {
  cat <<'USAGE'
install.sh — install the `tcr` CLI, and on macOS TcrBar too.

Env:
  TCR_SKIP_UI=1   Skip the TcrBar install step entirely (CLI only, all
                  platforms). Also honoured non-interactively so CI and
                  containers never try to touch /Applications.
  TCR_APPLICATIONS_DIR
                  Where TcrBar.app is installed and looked for (default
                  /Applications). Test-only — a real machine has exactly one.
  TCR_DMG_INSTALL_SCRIPT_URL
                  Where to fetch scripts/install-tcrbar-from-dmg.sh from when
                  this script has no local checkout to find it in, which is
                  every `curl | sh` run. Defaults to the raw file on main.
  TCR_SKIP_CLI=1  Skip the tcr CLI install step (step 1). Test-only — lets
                  the TcrBar half of this script be exercised without a real
                  network round trip through the cargo-dist installer.
  TEAMCLAUDE_RS_INSTALL_DIR, TEAMCLAUDE_RS_DOWNLOAD_URL,
  TEAMCLAUDE_RS_INSTALLER_GHE_BASE_URL,
  TEAMCLAUDE_RS_INSTALLER_GITHUB_BASE_URL, TEAMCLAUDE_RS_GITHUB_TOKEN
                  Passed through to the cargo-dist shell installer
                  unmodified (see src/update.rs for what each one does).
USAGE
}

case "${1:-}" in
  --help | -h)
    usage
    exit 0
    ;;
esac

if [ "${TCR_SKIP_CLI:-0}" = "1" ]; then
  echo "==> TCR_SKIP_CLI=1 — skipping the tcr CLI install (test-only)."
else
  echo "==> Installing the tcr CLI…"
  curl --proto '=https' --tlsv1.2 -LsSf "$DIST_INSTALLER_URL" | sh
  dist_rc="${PIPESTATUS[1]:-1}"
  if [ "$dist_rc" -ne 0 ]; then
    echo "tcr CLI install failed (exit $dist_rc)" >&2
    exit "$dist_rc"
  fi
fi

if [ "$(uname -s)" != "Darwin" ]; then
  exit 0
fi

if [ "${TCR_SKIP_UI:-0}" = "1" ]; then
  echo "==> TCR_SKIP_UI=1 — skipping the TcrBar install."
  exit 0
fi

if [ -d "${APPLICATIONS_DIR}/${APP_NAME}.app" ] && pgrep -x "$APP_NAME" >/dev/null 2>&1; then
  echo "==> ${APP_NAME} is already installed and running."
  echo "    Updates come through the app itself (Sparkle) — nothing more to do here."
  exit 0
fi

# validate_release_tag, mirrored from src/update.rs so this shell installer
# and the Rust update path agree on what a safe tag looks like. The tag is
# interpolated into a URL path below, and an unvalidated one does not stay
# inside dhkts1/teamclaude-rs: see update.rs's own doc-comment for the
# concrete escape it blocks.
validate_release_tag() {
  local t="$1"
  [ -n "$t" ] || return 1
  case "$t" in
    *[!0-9A-Za-z.+_-]*) return 1 ;;
  esac
  case "$t" in
    [0-9A-Za-z]*) return 0 ;;
    *) return 1 ;;
  esac
}

echo "==> Resolving the latest TcrBar release…"
release_json="$(curl -fsSL "$LATEST_RELEASE_API_URL")" || {
  echo "could not reach $LATEST_RELEASE_API_URL — skipping the TcrBar install" >&2
  exit 0
}
tag="$(printf '%s' "$release_json" | grep -o '"tag_name"[[:space:]]*:[[:space:]]*"[^"]*"' | head -1 | sed -E 's/.*"tag_name"[[:space:]]*:[[:space:]]*"([^"]*)".*/\1/')"

if ! validate_release_tag "$tag"; then
  echo "the latest-release API returned an unusable tag_name (${tag:-<empty>}) — skipping the TcrBar install" >&2
  exit 0
fi

version="${tag#v}"
dmg_url="${DMG_URL_BASE}/${tag}/${APP_NAME}-${version}.dmg"

tmp_dir="$(mktemp -d)"
trap 'rm -rf "$tmp_dir"' EXIT
dmg_path="$tmp_dir/${APP_NAME}.dmg"

echo "==> Downloading ${APP_NAME} ${tag}…"
if ! curl -fsSL -o "$dmg_path" "$dmg_url"; then
  echo "could not download $dmg_url — skipping the TcrBar install" >&2
  exit 0
fi

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
dmg_install_script="$script_dir/scripts/install-tcrbar-from-dmg.sh"
if [ ! -f "$dmg_install_script" ]; then
  # `curl | sh` has no checkout: ${BASH_SOURCE[0]} resolves under the pipe's
  # cwd, not this repo, so the sibling script this file used to assume is
  # simply not there. Fetch the one canonical copy (see the script's own
  # header — it is shared with `tcr ui` via include_str! and must stay one
  # copy, not two that drift) instead of re-implementing the mount/swap here.
  dmg_install_script="$tmp_dir/install-tcrbar-from-dmg.sh"
  echo "==> No local install-tcrbar-from-dmg.sh — fetching it…"
  if ! curl -fsSL -o "$dmg_install_script" "$DMG_INSTALL_SCRIPT_URL"; then
    echo "could not fetch $DMG_INSTALL_SCRIPT_URL — skipping the TcrBar install" >&2
    exit 0
  fi
  chmod +x "$dmg_install_script"
fi
TCR_APPLICATIONS_DIR="$APPLICATIONS_DIR" "$dmg_install_script" "$dmg_path"

echo ""
echo "Installed: ${APPLICATIONS_DIR}/${APP_NAME}.app"
echo "NOTE: ${APP_NAME} bundles its own tcr and prefers it over PATH, so"
echo "      \`tcr status --json\` run from the app may report a different"
echo "      build than \`which tcr\`."
