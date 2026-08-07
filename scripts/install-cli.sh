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
#
#   4. The source binary is checked against HEAD, not merely for existence.
#      `build.rs` stamps TCR_BUILD_SHA into the binary, so the file can be asked
#      which commit it came from. Without that check this script installed a
#      six-commits-old artifact and printed "installed" with exit 0 — reporting
#      success for a build it did not do, which is the one thing this whole
#      family of scripts exists to stop. apps/macos/scripts/build-tcrbar.sh and
#      .githooks/post-merge already grep for the same stamp; this now matches.
#
# KNOWN LIMIT: the symlink refusal in note 3 catches symlinks, not HARDLINKS. A
# hardlinked destination is detected by its link count where `stat` supports it
# (BSD and GNU are probed separately) and refused the same way, but on a system
# with neither `stat` form the check is skipped with a warning rather than
# silently passing — a hardlink shared with the app bundle would otherwise be
# split into two drifting inodes with no error at all.
set -euo pipefail

FORCE="${TCR_INSTALL_FORCE:-0}"
DEST=""
DEST_SET=0

usage() {
  cat <<'USAGE'
Install the release `tcr` binary onto PATH.

Usage: scripts/install-cli.sh [--force] [destination]

  destination   Full path to the installed FILE, not a directory.
                Default: ~/.local/bin/tcr
  --force       Install even when the destination is a symlink or hardlink, or
                when the built binary does not match HEAD.
                Equivalent to TCR_INSTALL_FORCE=1.
  -h, --help    Show this message.
USAGE
}

fail() { printf '!! %s\n' "$1" >&2; exit 1; }

# Unknown flags and surplus positionals are refused rather than absorbed.
# Previously every non-`--force` token became the destination, so `--help` was
# treated as a path (and died inside `dirname` with `illegal option`), and
# `install-cli.sh a b` silently installed to `b` while looking like it honoured
# `a`. Both are the same defect: an argument the script did not understand,
# accepted anyway.
for arg in "$@"; do
  case "$arg" in
    --force)
      FORCE=1
      ;;
    -h | --help)
      usage
      exit 0
      ;;
    -*)
      usage >&2
      fail "unknown option: $arg"
      ;;
    *)
      if [ "$DEST_SET" = "1" ]; then
        usage >&2
        fail "too many arguments -- one destination only (got '$DEST' and '$arg')"
      fi
      DEST="$arg"
      DEST_SET=1
      ;;
  esac
done
DEST="${DEST:-$HOME/.local/bin/tcr}"

command -v cargo >/dev/null 2>&1 || fail "cargo not found on PATH"
command -v jq    >/dev/null 2>&1 || fail "jq not found on PATH"

TARGET_DIR="$(cargo metadata --format-version 1 --no-deps | jq -r .target_directory)"
[ -n "$TARGET_DIR" ] && [ "$TARGET_DIR" != "null" ] || fail "could not resolve cargo target directory"
SRC="$TARGET_DIR/release/tcr"

[ -f "$SRC" ] || fail "no release binary at $SRC -- build it first:  cargo build --release"
[ -x "$SRC" ] || fail "$SRC is not executable"

# See note 4 in the header. `-f` proves a file is THERE, never that it is the
# one this checkout describes; a binary built six commits ago satisfies it just
# as well. build.rs stamps the short sha as TCR_BUILD_SHA, so grep the artifact
# and make it a fact. Same test, same stamp, as apps/macos/scripts/build-tcrbar.sh
# and .githooks/post-merge.
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
expected_sha="$(git -C "$repo_root" rev-parse --short HEAD 2>/dev/null || echo unknown)"
if [ "$expected_sha" = "unknown" ]; then
  echo "note: no git sha available -- cannot verify $SRC matches this checkout." >&2
elif grep -aq "$expected_sha" "$SRC"; then
  : # the built binary carries this checkout's sha
elif [ "$FORCE" = "1" ]; then
  echo "note: $SRC does not carry HEAD's sha $expected_sha -- installing anyway (--force)." >&2
else
  fail "$(printf '%s\n' \
    "stale binary -- refusing to install" \
    "  $SRC" \
    "does not carry this checkout's build sha $expected_sha, so it was built from" \
    "a DIFFERENT commit. Installing it would ship old code under a new version and" \
    "every 'the fix is live' claim afterwards would be false." \
    "Rebuild first:" \
    "  cargo build --release" \
    "then re-run this script. To install the stale binary on purpose:" \
    "  scripts/install-cli.sh --force  (TCR_INSTALL_FORCE=1)")"
fi

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

# The destination is a FILE path. Given a directory, `mv -f "$TMP" "$DEST"`
# succeeds by moving the temp file INTO it under its hidden `.tcr.install.XXXXXX`
# name -- so `scripts/install-cli.sh ~/.local/bin` exited 0, printed "installed",
# and left a 10MB dotfile nothing will ever exec. Refuse instead, and say what
# the argument should have been.
if [ -d "$DEST" ]; then
  fail "$(printf '%s\n' \
    "destination is a directory -- refusing to install" \
    "  $DEST" \
    "This argument is the full path of the installed FILE, not the directory to" \
    "put it in. Moving into a directory would land the binary under the installer's" \
    "own hidden temp name, which nothing on your PATH would ever find." \
    "Did you mean:" \
    "  scripts/install-cli.sh ${DEST%/}/tcr")"
fi

# See the KNOWN LIMIT note in the header: `-L` is blind to hardlinks, which
# split a shared app/CLI inode just as silently (measured: nlink 2 -> 1, no
# error). Link count is the only portable-ish signal; BSD and GNU `stat` spell
# it differently, and a system with neither gets a warning rather than a pass.
if [ -f "$DEST" ] && [ ! -L "$DEST" ] && [ "$FORCE" != "1" ]; then
  dest_links="$(stat -f %l "$DEST" 2>/dev/null || stat -c %h "$DEST" 2>/dev/null || echo "")"
  if [ -z "$dest_links" ]; then
    echo "note: no usable \`stat\` -- cannot check whether $DEST is hardlinked." >&2
  elif [ "$dest_links" -gt 1 ]; then
    fail "$(printf '%s\n' \
      "hardlinked destination ($dest_links links) -- refusing to install" \
      "  $DEST" \
      "It shares an inode with at least one other path, most likely the TcrBar.app" \
      "copy documented in apps/macos/README.md. This installer renames a new file" \
      "over the destination, which gives it a NEW inode and breaks that sharing" \
      "silently -- the app and the CLI would drift apart from the next build on." \
      "Instead:" \
      "  update the bundle:  apps/macos/scripts/install.sh" \
      "  or install elsewhere:  scripts/install-cli.sh /some/other/path/tcr" \
      "  or override on purpose:  scripts/install-cli.sh --force  (TCR_INSTALL_FORCE=1)")"
  fi
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
