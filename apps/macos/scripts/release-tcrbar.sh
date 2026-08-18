#!/usr/bin/env bash
# Cut a distributable TcrBar.app release: build → Developer ID signature →
# hardened runtime → DMG → notarize → staple → Sparkle EdDSA signature →
# appcast → GitHub Release.
#
# This is the DISTRIBUTION path. `build-tcrbar.sh` is the LOCAL path and stays
# that way: it signs with whatever certificate happens to be on the machine and
# says so, which is right for a developer build and wrong for something a
# stranger downloads. This script calls it and then refuses to continue unless
# the result is something Gatekeeper will actually open. It never reimplements
# the build.
#
# NOTHING HERE PRINTS A SECRET. This repository is public, and the strings that
# pass through this script — a certificate's common name, an App Store Connect
# issuer id, a team id, a Sparkle private key — are either credentials or
# personal data. Two consequences that look like paranoia and are not:
#
#   * The signing identity is never echoed. `codesign -dvv` reports it as
#     `Authority=Developer ID Application: Some Human (TEAMID)`; only the part
#     BEFORE the first `: ` (the certificate CLASS) is ever printed, because the
#     part after it is a name, an email and a team id. Every assert below
#     reports the class and swallows the rest.
#   * Sparkle's private key is handed to `sign_update` through a 0600 file, not
#     through `-s <key>`. Command-line arguments are world-readable in `ps`.
#
# `set -x` in this script would defeat both. Do not add it.
#
# Usage:
#   release-tcrbar.sh [--dry-run] [--skip-notarize] [--tag vX.Y.Z]
#   release-tcrbar.sh --verify-only /path/to/TcrBar.app
#
#   --dry-run       do everything except notarize and upload. This is how the
#                   pipeline is exercised on a machine with no credentials.
#                   Leaves apps/macos/appcast.xml byte-identical: the appcast
#                   item is built and validated against a scratch copy, never
#                   the tracked file.
#   --skip-notarize skip notarization and stapling only. The DMG is still built
#                   and signed; it will be Gatekeeper-quarantined on download.
#   --tag           the release tag. Defaults to v<Cargo.toml version> and must
#                   agree with it — two version numbers for one release is a bug
#                   this script exists to prevent, not to tolerate.
#   --verify-only   run the signature asserts (stages 2 and 3) against an
#                   already-built bundle and exit. Builds nothing.
#
# Exit status: 0 whenever publishing itself succeeded, including the case
# where apps/macos/appcast.xml is left uncommitted afterward — that is an
# outstanding manual step, not a failed release, and the closing message
# says so loudly (grep for "UNCOMMITTED") rather than the exit code doing it.
#
# Environment:
#   APPLE_API_KEY_PATH    path to the App Store Connect .p8 private key
#   APPLE_API_KEY_ID      the key id shown beside it
#   APPLE_API_ISSUER_ID   the issuer id shown at the top of that page
#   SPARKLE_ED_PRIVATE_KEY  Sparkle EdDSA private key (falls back to keychain)
#   SPARKLE_SIGN_UPDATE   path to Sparkle's sign_update, if not discoverable
#   RELEASE_REPO          owner/name for the GitHub Release (default: origin)
#
# See docs/RELEASING.md for how each of those is produced.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
pkg_dir="$(dirname "$here")"
repo_root="$(cd "$pkg_dir/../.." && pwd)"

app_name="TcrBar"
build_dir="$pkg_dir/build"
app_dir="$build_dir/$app_name.app"
appcast_path="$pkg_dir/appcast.xml"

# The one class of certificate that can sign a directly-downloaded Mac app.
#
# Not "Apple Development" (a development cert; Gatekeeper blocks it on every
# machine except the ones in its provisioning profile), not "Apple
# Distribution" (Mac App Store only), not "Developer ID Installer" (signs .pkg
# installers, which we do not ship). See docs/RELEASING.md.
readonly required_cert_class="Developer ID Application"

die() { printf 'ERROR: %s\n' "$@" >&2; exit 1; }
stage() { printf '\n==> %s\n' "$1"; }
note() { printf '    %s\n' "$1"; }

# ---------------------------------------------------------------------------
# Stage 2 — the Developer ID assert
# ---------------------------------------------------------------------------

# Pure predicate: is this certificate CLASS one that Gatekeeper honours for a
# direct download? Separated from the codesign call on purpose — it is the only
# part of the assert that can be exercised without a certificate, and a check
# that cannot be tested is a check nobody has tested.
cert_class_is_distributable() {
  [ "${1:-}" = "$required_cert_class" ]
}

# Read the signing certificate CLASS out of a bundle. Prints the class only
# (e.g. `Developer ID Application`), never the holder's name/email/team id.
# Prints `ad-hoc` for an ad-hoc signature and nothing at all when unsigned.
bundle_cert_class() {
  local bundle="$1" out authority
  # codesign writes everything to stderr.
  out="$(codesign -dvv "$bundle" 2>&1)" || return 1
  authority="$(printf '%s\n' "$out" | grep -m1 '^Authority=' || true)"
  if [ -z "$authority" ]; then
    if printf '%s\n' "$out" | grep -q '^Signature=adhoc'; then
      printf 'ad-hoc\n'
    fi
    return 0
  fi
  # `Authority=Developer ID Application: Some Human (TEAMID)` -> the class.
  # No `: ` at all means a class-only authority; keep the whole thing.
  authority="${authority#Authority=}"
  printf '%s\n' "${authority%%: *}"
}

assert_developer_id() {
  local bundle="$1" class
  [ -d "$bundle" ] || die "no bundle at $bundle — did the build stage run?"
  class="$(bundle_cert_class "$bundle")" \
    || die "codesign could not read a signature from $bundle."

  if cert_class_is_distributable "$class"; then
    note "signature: $required_cert_class (holder withheld — this repo is public)"
    return 0
  fi

  {
    echo "ERROR: $bundle is signed with: ${class:-<unsigned>}"
    echo "ERROR: a release MUST be signed with a '$required_cert_class'"
    echo "ERROR: certificate. Anything else is Gatekeeper-blocked on every Mac"
    echo "ERROR: except the ones this certificate was issued for, so shipping it"
    echo "ERROR: would produce a download that silently refuses to open."
    echo "ERROR:"
    echo "ERROR: Create the certificate — this is the exact one, the names are"
    echo "ERROR: close enough to pick the wrong one:"
    echo "ERROR:   1. https://developer.apple.com/account/resources/certificates"
    echo "ERROR:   2. '+' -> Software -> 'Developer ID Application'"
    echo "ERROR:      NOT 'Developer ID Installer' (that signs .pkg, we ship a DMG)"
    echo "ERROR:      NOT 'Apple Distribution'     (Mac App Store only)"
    echo "ERROR:      NOT 'Apple Development'      (a local development cert)"
    echo "ERROR:   3. requires the Account Holder role on the Apple Developer team"
    echo "ERROR:   4. upload a CSR from Keychain Access ->"
    echo "ERROR:      Certificate Assistant -> Request a Certificate From a"
    echo "ERROR:      Certificate Authority, then download and double-click the"
    echo "ERROR:      resulting .cer to install it in the login keychain"
    echo "ERROR:   5. confirm with: security find-identity -v -p codesigning"
    echo "ERROR:      (a line reading \"$required_cert_class: ...\")"
    echo "ERROR:"
    echo "ERROR: build-tcrbar.sh will then pick it automatically — its signing"
    echo "ERROR: ladder prefers $required_cert_class over Apple Development."
  } >&2
  exit 1
}

# ---------------------------------------------------------------------------
# Stage 3 — hardened runtime
# ---------------------------------------------------------------------------

# Notarization requires the hardened runtime, and `notarytool` rejects a bundle
# without it with a message that does not name the cause. build-tcrbar.sh signs
# without it (correct for a local build — the hardened runtime blocks debugger
# attach), so the release path re-signs. Nested Mach-O first: the outer
# signature seals the bundle's contents, so re-signing the inner binary after
# the outer one would invalidate the outer one.
#
# --timestamp is LOAD-BEARING here, not hygiene. It contacts Apple's timestamp
# server and records when the signature was made. A timestamped signature stays
# valid after the signing certificate expires; an untimestamped one becomes
# invalid the moment the certificate does, retroactively, on every machine that
# already downloaded it. Developer ID certificates issued for this project have
# run as short as six months, so an untimestamped build would break well inside
# the lifetime of an installed copy. Every codesign call in this file — nested
# binary, bundle, DMG — carries it.

# Prints the full signing identity string. Never log the result: it contains the
# certificate holder's name and the team id.
find_signing_identity() {
  security find-identity -v -p codesigning 2>/dev/null \
    | grep -F "\"$required_cert_class: " \
    | head -n 1 \
    | sed -E 's/^[^"]*"(.*)"[[:space:]]*$/\1/' || true
}

resign_hardened() {
  local bundle="$1" identity
  identity="$(find_signing_identity)"
  [ -n "$identity" ] || die \
    "no '$required_cert_class' identity in the keychain — cannot sign a release." \
    "Check with: security find-identity -v -p codesigning" \
    "Create it at https://developer.apple.com/account/resources/certificates" \
    "('+' -> Software -> '$required_cert_class'; NOT Developer ID Installer," \
    "NOT Apple Distribution, NOT Apple Development)."

  codesign --force --options runtime --timestamp \
    --sign "$identity" "$bundle/Contents/MacOS/tcr" >/dev/null 2>&1 \
    || die "codesign failed on the nested tcr binary."
  codesign --force --options runtime --timestamp \
    --sign "$identity" "$bundle" >/dev/null 2>&1 \
    || die "codesign failed on $bundle."
}

assert_hardened_runtime() {
  local bundle="$1" desc
  codesign --verify --deep --strict --verbose=2 "$bundle" >/dev/null 2>&1 \
    || die "codesign --verify --deep --strict failed on $bundle."
  # Captured, then matched — NOT `codesign ... | grep -q`. Under `set -o
  # pipefail` a `grep -q` that finds its match exits immediately, SIGPIPEs
  # codesign, and the pipeline reports 141: the check fails precisely when it
  # succeeds. This assert did exactly that on its first run against a correctly
  # hardened bundle.
  desc="$(codesign -dvv "$bundle" 2>&1)"
  case "$desc" in
    *"flags="*"runtime"*) ;;
    *) die "$bundle is not signed with the hardened runtime (--options runtime)." \
           "notarytool would reject it without naming this as the cause." ;;
  esac
  note "hardened runtime: present, signature verifies --deep --strict"
}

# ---------------------------------------------------------------------------
# Stage 3.5 — Sparkle public key assert
# ---------------------------------------------------------------------------

# build-tcrbar.sh only WARNS when TCRBAR_SPARKLE_PUBLIC_KEY is unset — right
# for a local build, where the warning is right there on the screen you are
# watching. This is the distribution path: the warning scrolls past into a
# log nobody re-reads, and the built app carries no SUPublicEDKey. That app
# CAN check the feed but its Sparkle instance will refuse to install anything
# it finds there, because it has no key to verify the download against — a
# one-way trap that needs a manual reinstall to escape. Refuse here, before a
# DMG of that app exists, rather than warn and let it ship.
assert_sparkle_public_key() {
  local bundle="$1" key
  key="$(/usr/libexec/PlistBuddy -c 'Print :SUPublicEDKey' "$bundle/Contents/Info.plist" 2>/dev/null || true)"
  if [ -n "$key" ]; then
    note "SUPublicEDKey: present in Info.plist"
    return 0
  fi
  {
    echo "ERROR: $bundle/Contents/Info.plist has no SUPublicEDKey."
    echo "ERROR: the app inside this bundle can check the Sparkle feed but will"
    echo "ERROR: refuse to install anything it finds there — it has no key to"
    echo "ERROR: verify a downloaded update against. Every installed copy would"
    echo "ERROR: need a manual reinstall to recover; this is a one-way trap."
    echo "ERROR:"
    echo "ERROR: Fix: export TCRBAR_SPARKLE_PUBLIC_KEY=<the EdDSA public key> and"
    echo "ERROR: rebuild, or export TCRBAR_OP_ITEM so release-local.sh can fetch"
    echo "ERROR: it from 1Password before calling this script."
  } >&2
  exit 1
}

# ---------------------------------------------------------------------------
# Sparkle
# ---------------------------------------------------------------------------

find_sign_update() {
  if [ -n "${SPARKLE_SIGN_UPDATE:-}" ]; then
    [ -x "$SPARKLE_SIGN_UPDATE" ] || die "SPARKLE_SIGN_UPDATE=$SPARKLE_SIGN_UPDATE is not executable."
    printf '%s\n' "$SPARKLE_SIGN_UPDATE"
    return 0
  fi
  # Sparkle ships sign_update inside its SPM artifact bundle, which lands under
  # .build/artifacts once the package has been resolved and built.
  # `|| true`: `head` closing the pipe SIGPIPEs `find`, and under pipefail that
  # non-zero status would abort the script on the normal path.
  local found
  found="$(find "$pkg_dir/.build/artifacts" -type f -name sign_update -perm -u+x 2>/dev/null | head -n 1 || true)"
  [ -n "$found" ] || die \
    "could not find Sparkle's sign_update under $pkg_dir/.build/artifacts." \
    "It ships in the Sparkle SPM artifact bundle — build the package first," \
    "or set SPARKLE_SIGN_UPDATE to its path."
  printf '%s\n' "$found"
}

# Sets `sparkle_sig_attrs` to `sparkle:edSignature="..." length="..."`, exactly
# the attribute pair the appcast <enclosure> needs.
#
# It sets a variable instead of printing, and the caller must NOT wrap it in
# `$(...)`. A `die` inside a command substitution exits only that subshell: the
# first version of this printed its signing-tool-not-found error and then
# carried straight on into the fallback branch, reporting a SECOND failure for a
# stage that should already have aborted. An abort that does not abort is worse
# than no check.
sparkle_sig_attrs=""
sparkle_sign() {
  local dmg="$1" tool sig keyfile
  tool="$(find_sign_update)" || exit 1
  [ -n "$tool" ] || exit 1
  if [ -n "${SPARKLE_ED_PRIVATE_KEY:-}" ]; then
    # A 0600 file, not `-s <key>`: process arguments are readable by every user
    # on the machine via `ps`, and a CI runner is a machine like any other.
    keyfile="$(mktemp)"
    chmod 600 "$keyfile"
    printf '%s' "$SPARKLE_ED_PRIVATE_KEY" >"$keyfile"
    sig="$("$tool" -f "$keyfile" "$dmg" 2>/dev/null)" || { rm -f "$keyfile"; die "sign_update failed."; }
    rm -f "$keyfile"
  else
    # No env key: sign_update reads the private key from the login keychain,
    # which is where `generate_keys` put it.
    sig="$("$tool" "$dmg" 2>/dev/null)" \
      || die "sign_update failed and SPARKLE_ED_PRIVATE_KEY is unset." \
             "Either run on the machine holding the key in its login keychain," \
             "or export it with: generate_keys -x -"
  fi
  [ -n "$sig" ] || die "sign_update produced an empty signature."
  sparkle_sig_attrs="$sig"
}

# ---------------------------------------------------------------------------
# Appcast
# ---------------------------------------------------------------------------

readonly appcast_marker='<!-- release-tcrbar.sh inserts new items directly below this line -->'

ensure_appcast() {
  local target="$1"
  [ -f "$target" ] && return 0
  cat >"$target" <<XML
<?xml version="1.0" encoding="utf-8"?>
<rss version="2.0" xmlns:sparkle="http://www.andymatuschak.org/xml-namespaces/sparkle">
  <channel>
    <title>TcrBar</title>
    <description>Updates for TcrBar</description>
    <language>en</language>
    $appcast_marker
  </channel>
</rss>
XML
  note "appcast: created $target"
}

# Insert the item at a FIXED marker rather than "after <channel>" or "before
# </channel>". Sparkle reads items newest-first, and an insertion point derived
# by pattern-matching the surrounding XML silently moves the day someone
# reformats the file. A missing marker aborts instead of guessing.
#
# The item arrives through a FILE, not `awk -v item=...`. BSD awk (the one on
# macOS) rejects a literal newline inside a -v assignment — `awk: newline in
# string` — and the item is a multi-line <item> block, so -v aborted awk before
# it read a single input line. That wrote an EMPTY tmp file over the appcast:
# exit status 0, a feed with no items and no marker, and Sparkle telling every
# user they are up to date forever. Hence also the post-insert assert below: a
# silent, well-formed, wrong result is the failure mode this function has.
appcast_insert() {
  local target="$1" item="$2" tmp itemfile before after
  grep -qF "$appcast_marker" "$target" \
    || die "$appcast_path has no insertion marker. Restore this line inside <channel>:" \
           "  $appcast_marker"
  before="$(grep -c '<item>' "$target" || true)"
  tmp="$(mktemp)"
  itemfile="$(mktemp)"
  printf '%s\n' "$item" >"$itemfile"
  awk -v marker="$appcast_marker" -v itemfile="$itemfile" '
    { print }
    index($0, marker) && !done {
      while ((getline line < itemfile) > 0) print line
      close(itemfile)
      done = 1
    }
  ' "$target" >"$tmp" || { rm -f "$tmp" "$itemfile"; die "appcast: awk failed to insert the item."; }
  rm -f "$itemfile"
  after="$(grep -c '<item>' "$tmp" || true)"
  [ "$after" -eq $((before + 1)) ] \
    || { rm -f "$tmp"; die "appcast: insert produced $after <item> elements, expected $((before + 1))." \
                           "Refusing to publish a feed that lost or duplicated a release."; }
  mv "$tmp" "$target"
}

# ---------------------------------------------------------------------------
# Feed-outage guard
# ---------------------------------------------------------------------------

# Measured across four releases (docs/plans/swarm-retrospectives.md,
# 2026-08-10 entry: 11m37s outage; prevented on 2026-08-14 only by a
# disposable ad-hoc script) the update feed goes dark for the entire gap
# between the tag-triggered workflow creating the GitHub Release — which
# becomes `latest` immediately, carrying only CLI tarballs — and THIS script
# reaching stage 9 to upload the DMG and appcast, which is after the build,
# sign, DMG and (usually) notarize stages. Waiting until stage 9 to look is
# what left the window open: by the time stage 9's own wait-loop finds the
# release, it may already have been live and assetless for as long as the
# build took.
#
# `releases/latest/download/appcast.xml` is the URL Sparkle fetches; a
# release that IS `latest` and has no appcast.xml on it 404s every install's
# update check, and GitHub's CDN caches that 404 against the exact URL.
#
# Reports whether $tag's Release exists and is assetless (no appcast.xml on
# it yet) — the exact state stage 9 already knows how to mark prerelease.
# Prints nothing; callers act on the exit code. Treats "could not ask" as
# "nothing to quarantine" rather than failing the release outright — this
# guard runs unattended in the background during the build and must never be
# the thing that aborts a release over a transient network blip.
release_is_assetless() {
  local tag="$1" repo="$2" assets
  assets="$(gh api "repos/$repo/releases/tags/$tag" --jq '[.assets[].name] | join(" ")' 2>/dev/null)" || return 1
  case " $assets " in
    *" appcast.xml "*) return 1 ;;
    *) return 0 ;;
  esac
}

# Marks $tag prerelease so `latest` falls back to the previous good release.
# Idempotent — `gh release edit` succeeds whether or not it was already
# prerelease — so calling this repeatedly from a poll loop is safe. Failure
# is swallowed rather than fatal for the same "must not abort a release over
# a network blip" reason as release_is_assetless above; stage 9's own
# explicit call (not this one) is the one whose failure IS fatal, because by
# then it is the last chance before upload.
quarantine_if_assetless() {
  local tag="$1" repo="$2"
  release_is_assetless "$tag" "$repo" || return 0
  gh release edit "$tag" --prerelease --repo "$repo" >/dev/null 2>&1 || true
}

# Runs quarantine_if_assetless in the background every 10s for the whole
# build/sign/notarize duration — the actual window this guard exists to
# close. Sets `quarantine_watcher_pid`; stop_quarantine_watcher tears it down.
# The subshell has its own `set +e`: a background loop must never let one
# failed `gh` call kill itself, since nothing would then be watching for the
# rest of the build.
quarantine_watcher_pid=""
start_quarantine_watcher() {
  local tag="$1" repo="$2"
  (
    set +e
    while true; do
      quarantine_if_assetless "$tag" "$repo"
      sleep 10
    done
  ) &
  quarantine_watcher_pid=$!
}

stop_quarantine_watcher() {
  [ -n "$quarantine_watcher_pid" ] || return 0
  kill "$quarantine_watcher_pid" >/dev/null 2>&1 || true
  wait "$quarantine_watcher_pid" 2>/dev/null || true
  quarantine_watcher_pid=""
}

# ---------------------------------------------------------------------------

usage() { sed -n '2,60p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; }

main() {
  local dry_run=0 skip_notarize=0 tag="" verify_only="" appcast_target=""
  while [ $# -gt 0 ]; do
    case "$1" in
      --dry-run)       dry_run=1 ;;
      --skip-notarize) skip_notarize=1 ;;
      --tag)           tag="${2:-}"; shift ;;
      --verify-only)   verify_only="${2:-}"; shift ;;
      -h|--help)       usage; exit 0 ;;
      *)               die "unknown argument: $1" ;;
    esac
    shift
  done

  if [ -n "$verify_only" ]; then
    stage "verify-only: $verify_only"
    assert_developer_id "$verify_only"
    assert_hardened_runtime "$verify_only"
    assert_sparkle_public_key "$verify_only"
    exit 0
  fi

  local version
  version="$(sed -n 's/^version[[:space:]]*=[[:space:]]*"\([0-9][^"]*\)".*/\1/p' "$repo_root/Cargo.toml" | head -1)"
  [ -n "$version" ] || die "could not read version from $repo_root/Cargo.toml."
  if [ -z "$tag" ]; then
    tag="v$version"
  elif [ "$tag" != "v$version" ]; then
    die "--tag $tag disagrees with Cargo.toml version $version." \
        "One release must not carry two version numbers; bump Cargo.toml or fix the tag."
  fi

  local repo
  repo="${RELEASE_REPO:-$(gh repo view --json nameWithOwner -q .nameWithOwner 2>/dev/null || true)}"
  [ -n "$repo" ] || die "could not determine the GitHub repository; set RELEASE_REPO=owner/name."

  local dmg="$build_dir/$app_name-$version.dmg"

  # Start the feed-outage guard now, before the build even begins: the tag is
  # normally pushed just before this script is invoked (see docs/RELEASING.md),
  # so the tag-triggered Release can already exist by the time we get here, and
  # the build/sign/notarize stages below are the whole outage window this
  # guard exists to close. A --dry-run never touches a real GitHub Release
  # (nothing is uploaded, stage 9 is skipped), so there is nothing to
  # quarantine and starting the watcher would just poll a real repo for no
  # reason — skip it.
  if [ "$dry_run" != 1 ]; then
    start_quarantine_watcher "$tag" "$repo"
  fi

  # One trap, set unconditionally, that always runs on every exit path
  # (success, `die`, an interrupted build) rather than two competing ones. It
  # is safe to call stop_quarantine_watcher even when no watcher was started
  # — it no-ops on an empty $quarantine_watcher_pid — and it is safe to `rm`
  # $appcast_target even when stage 8 never ran or never set it, because that
  # happens with the shell's own unset-variable check under `set -u`, which
  # is why the guard below reads it through `${appcast_target:-}` rather than
  # assuming stage 8 already ran.
  #
  # $appcast_target is only ever a scratch mktemp file when it differs from
  # $appcast_path (the --dry-run case); the real file is never `rm`'d here.
  trap '
    stop_quarantine_watcher
    if [ -n "${appcast_target:-}" ] && [ "$appcast_target" != "$appcast_path" ]; then
      rm -f "$appcast_target"
    fi
  ' EXIT

  # ---- stage 1: build -----------------------------------------------------
  stage "stage 1/9  build (apps/macos/scripts/build-tcrbar.sh)"
  "$here/build-tcrbar.sh"

  # ---- stage 2: Developer ID assert ---------------------------------------
  stage "stage 2/9  assert the bundle is signed for distribution"
  assert_developer_id "$app_dir"

  # ---- stage 3: hardened runtime ------------------------------------------
  stage "stage 3/9  re-sign with the hardened runtime and a secure timestamp"
  resign_hardened "$app_dir"
  assert_hardened_runtime "$app_dir"

  # ---- stage 3.5: Sparkle public key assert -------------------------------
  stage "stage 3.5/9  assert the bundle carries a Sparkle public key"
  assert_sparkle_public_key "$app_dir"

  # ---- stage 4: DMG -------------------------------------------------------
  stage "stage 4/9  build the DMG"
  command -v create-dmg >/dev/null 2>&1 \
    || die "create-dmg is not installed. Install it with: brew install create-dmg"
  rm -f "$dmg"
  # create-dmg exits 2 when it built the image but could not set the window
  # background; that is cosmetic and not a release failure.
  create-dmg \
    --volname "$app_name $version" \
    --window-size 520 340 \
    --icon-size 96 \
    --icon "$app_name.app" 130 170 \
    --app-drop-link 390 170 \
    --hdiutil-quiet \
    "$dmg" "$app_dir" || true

  # FALLBACK — because the tolerant `|| [ -f "$dmg" ]` above is not enough.
  #
  # create-dmg styles the image by driving Finder over AppleScript. That step can
  # fail OUTRIGHT rather than cosmetically, and when it does it aborts before the
  # image is finalised — so there is no file left to tolerate and the whole release
  # dies on presentation. Measured 2026-08-15 on v0.2.12, twice in a row, with
  # Finder reachable and the screen unlocked:
  #
  #   execution error: Finder got an error: Can't set statusbar visible of
  #   container window of disk "dmg.bB4bq2" to false. (-10006)
  #
  # Nothing downstream cares what the window looks like: codesign, notarytool,
  # stapler and Sparkle all operate on the image, not on its Finder presentation.
  # Losing a release to a drag-and-drop background is the wrong trade, so fall back
  # to a plain image — and say so loudly, because the DMG a user opens will look
  # different from every previous one and that must not be a silent change.
  #
  # The staging dir reproduces the one affordance that matters, the Applications
  # symlink, so the fallback image is still usable by hand. `ditto` rather than
  # `cp -R`: it preserves the extended attributes and the code signature, which a
  # naive copy can strip — and an unsigned app inside a signed image fails
  # notarization for a reason that looks nothing like its cause.
  if [ ! -f "$dmg" ]; then
    printf 'WARNING: create-dmg produced no image — falling back to an UNSTYLED hdiutil image.\n' >&2
    printf 'WARNING:   the drag-to-Applications window layout will be missing.\n' >&2
    printf 'WARNING:   signing, notarization, stapling and the Sparkle feed are unaffected.\n' >&2
    dmg_stage="$(mktemp -d)"
    ditto "$app_dir" "$dmg_stage/$(basename "$app_dir")" \
      || die "could not stage $app_dir for the fallback image."
    ln -s /Applications "$dmg_stage/Applications" \
      || die "could not create the Applications symlink for the fallback image."
    hdiutil create -quiet -srcfolder "$dmg_stage" -volname "$app_name $version" \
      -fs HFS+ -format UDZO "$dmg" \
      || { rm -rf "$dmg_stage"; die "hdiutil could not create $dmg either."; }
    rm -rf "$dmg_stage"
  fi
  [ -f "$dmg" ] || die "no image at $dmg after create-dmg and the hdiutil fallback."
  note "dmg: $dmg"

  # The DMG is signed too: an unsigned disk image gets its own Gatekeeper
  # complaint even when the app inside it is perfectly signed. --timestamp for
  # the reason given above the signing helpers — the signature has to outlive
  # the certificate.
  codesign --force --timestamp --sign "$(find_signing_identity)" "$dmg" >/dev/null 2>&1 \
    || die "could not sign $dmg."

  # ---- stage 5: notarize --------------------------------------------------
  if [ "$dry_run" = 1 ] || [ "$skip_notarize" = 1 ]; then
    stage "stage 5/9  notarize — SKIPPED"
    note "the DMG is NOT notarized; macOS will refuse to open it after download."
  else
    stage "stage 5/9  notarize (xcrun notarytool submit --wait)"
    : "${APPLE_API_KEY_PATH:?set APPLE_API_KEY_PATH to the App Store Connect .p8 file}"
    : "${APPLE_API_KEY_ID:?set APPLE_API_KEY_ID}"
    : "${APPLE_API_ISSUER_ID:?set APPLE_API_ISSUER_ID}"
    [ -f "$APPLE_API_KEY_PATH" ] || die "APPLE_API_KEY_PATH does not point at a file."
    # An API key, not an Apple ID + app-specific password: the password pair is
    # tied to one human's account and stops working the moment they enable a
    # different 2FA device, which is a release outage nobody can debug at 2am.
    xcrun notarytool submit "$dmg" \
      --key "$APPLE_API_KEY_PATH" \
      --key-id "$APPLE_API_KEY_ID" \
      --issuer "$APPLE_API_ISSUER_ID" \
      --wait --timeout 30m \
      || die "notarization failed. Get the detail with:" \
             "  xcrun notarytool log <submission-id> --key ... --key-id ... --issuer ..."
  fi

  # ---- stage 6: staple ----------------------------------------------------
  if [ "$dry_run" = 1 ] || [ "$skip_notarize" = 1 ]; then
    stage "stage 6/9  staple — SKIPPED (nothing was notarized)"
  else
    stage "stage 6/9  staple the notarization ticket"
    # Stapling is what makes the DMG open on a machine that is OFFLINE. Without
    # it Gatekeeper has to ask Apple at open time, so a first launch without
    # network fails for a release that was notarized perfectly well.
    xcrun stapler staple "$dmg" || die "stapler staple failed on $dmg."
    xcrun stapler validate "$dmg" || die "stapler validate failed on $dmg."
    note "stapled and validated"
  fi

  # ---- stage 7: Sparkle signature -----------------------------------------
  stage "stage 7/9  Sparkle EdDSA signature"
  # Deliberately not `$(sparkle_sign ...)` — see the note on that function.
  sparkle_sign "$dmg"
  note "sign_update: ok (signature withheld from this log)"

  # ---- stage 8: appcast ---------------------------------------------------
  stage "stage 8/9  appcast entry"
  local build_number pubdate item url
  build_number="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleVersion' "$app_dir/Contents/Info.plist")"
  pubdate="$(LC_ALL=C date -u '+%a, %d %b %Y %H:%M:%S +0000')"
  url="https://github.com/$repo/releases/download/$tag/$(basename "$dmg")"

  # A --dry-run must leave the TRACKED $appcast_path byte-identical. It used
  # not to: stage 8 always ran ensure_appcast/appcast_insert against the real
  # file and only stage 9's upload was skipped under --dry-run, so a
  # rehearsal wrote a real <item> into the tracked file every time. That is
  # how a rehearsal poisons the following real run — the duplicate-item guard
  # below would then see the rehearsal's own entry and die (the documented
  # failure mode is dying here AFTER notarizing, the expensive stages). It is
  # also how the appcast entry twice ended up stranded uncommitted:
  # 0.2.15 (c47aa99, #119) and 0.2.17 (PR #121) each needed a follow-up
  # commit, and release-preflight.sh check 5/6 only ever catches this one
  # release too late. Below, ensure_appcast, the duplicate guard and
  # appcast_insert all run against $appcast_target — a scratch copy of the
  # real file under --dry-run, and the real file itself otherwise — so a
  # rehearsal builds and validates the exact same item without ever touching
  # the tracked file. $appcast_target was declared (empty) at the top of
  # main() so the EXIT trap can always see it, even if this stage dies before
  # reassigning it.
  #
  # `mktemp` CREATES its file, at zero bytes, before we ever get to decide
  # whether to seed it. Left as-is, that empty file makes `ensure_appcast`'s
  # `[ -f "$target" ] && return 0` treat a brand-new appcast as "already
  # there" and skip writing the RSS skeleton — so a rehearsal of a
  # first-ever release (no tracked appcast.xml yet) would die on "no
  # insertion marker" in exactly the case where the real run succeeds and
  # prints "appcast: created". `rm -f` the just-created empty file whenever
  # there is nothing real to seed it from, so `ensure_appcast` sees a genuine
  # absence and creates the skeleton — the same thing the real path does.
  appcast_target="$appcast_path"
  if [ "$dry_run" = 1 ]; then
    appcast_target="$(mktemp "${TMPDIR:-/tmp}/release-tcrbar-appcast-dry-run.XXXXXX")"
    if [ -f "$appcast_path" ]; then
      cp "$appcast_path" "$appcast_target"
    else
      rm -f "$appcast_target"
    fi
  fi

  ensure_appcast "$appcast_target"
  item="    <item>
      <title>$app_name $version</title>
      <link>https://github.com/$repo/releases/tag/$tag</link>
      <sparkle:version>$build_number</sparkle:version>
      <sparkle:shortVersionString>$version</sparkle:shortVersionString>
      <sparkle:minimumSystemVersion>13.0</sparkle:minimumSystemVersion>
      <pubDate>$pubdate</pubDate>
      <enclosure url=\"$url\" type=\"application/octet-stream\" $sparkle_sig_attrs />
    </item>"

  # The duplicate-item guard, derived from the bytes we are ABOUT TO WRITE.
  #
  # It used to be `grep -q "sparkle:shortVersionString=\"$version\""` — the
  # ATTRIBUTE form — sitting four lines above the `item` block that writes the
  # ELEMENT form. No item this script has ever produced could match it, so the
  # guard was dead from the day it was written and the "dies at stage 8" it
  # promises never once happened.
  #
  # Restating the shape in a second place is what allowed the drift, so this no
  # longer restates it. The needle is extracted from `$item` itself, matched with
  # grep -F, and the extraction is asserted non-empty — if the item's field names
  # ever change, this fails loudly rather than silently matching nothing.
  local version_needle
  version_needle="$(printf '%s\n' "$item" \
    | grep -o '<sparkle:shortVersionString>[^<]*</sparkle:shortVersionString>')"
  [ -n "$version_needle" ] \
    || die "internal: could not derive the version line from the appcast item." \
           "The item template changed; update the guard in stage 8."
  # No manual cleanup of $appcast_target here or below: the EXIT trap set
  # earlier in main() removes the scratch copy on every exit path, `die`
  # included, so a die() here can't leak it into $TMPDIR.
  if grep -qF "$version_needle" "$appcast_target"; then
    die "$appcast_path already has an item for version $version." \
        "Bump the version in Cargo.toml rather than republishing one."
  fi

  appcast_insert "$appcast_target" "$item"
  if [ "$dry_run" = 1 ]; then
    note "appcast: would add $version ($build_number) -> $appcast_path (dry run; $appcast_path left untouched)"
  else
    note "appcast: added $version ($build_number) -> $appcast_path"
  fi

  # ---- stage 9: publish ---------------------------------------------------
  if [ "$dry_run" = 1 ]; then
    stage "stage 9/9  publish — SKIPPED (--dry-run)"
    note "would upload $(basename "$dmg") and appcast.xml to $repo release $tag"
  else
    stage "stage 9/9  publish to the GitHub Release"

    # Stop the background guard here: from this point on, the fatal
    # gh-release-edit calls below are the ones responsible for the
    # prerelease/upload/reveal sequence, and having both running at once buys
    # nothing but a harder-to-read log.
    stop_quarantine_watcher

    # WAIT for the release to exist. It is a race, not a given.
    #
    # This used to upload immediately, on the reasoning that "the release
    # already exists (the CLI workflow creates it from the same tag)". The
    # workflow does create it — asynchronously, off the tag push, after
    # building every CLI target. So the release exists *eventually*, and
    # RELEASING.md tells you to push the tag and then run this script, which is
    # exactly the order that loses the race.
    #
    # Measured on v0.2.1: notarization finished, the ticket stapled, the appcast
    # entry was written, and stage 9 died on `release not found` while the
    # Release workflow was still in_progress. Everything expensive had already
    # succeeded; only the upload was lost, and re-running meant notarizing a
    # second time.
    #
    # Waiting rather than creating is deliberate. `gh release create` here would
    # race the workflow the other way and can leave TWO releases on one tag —
    # which happened on v0.2.0, and made `releases/latest/download/appcast.xml`
    # resolve to the wrong one and 404 the update feed.
    release_wait_seconds="${TCRBAR_RELEASE_WAIT:-600}"
    waited=0
    until gh release view "$tag" --repo "$repo" >/dev/null 2>&1; do
      if [ "$waited" -ge "$release_wait_seconds" ]; then
        die "release $tag still does not exist after ${release_wait_seconds}s." \
            "  The tag-triggered Release workflow creates it; check:" \
            "    gh run list --repo $repo --branch $tag" \
            "  The DMG is built, notarized and stapled at:" \
            "    $dmg" \
            "  Re-run with --verify-only, or upload by hand once it appears:" \
            "    gh release upload $tag '$dmg' '$appcast_path' --clobber --repo $repo"
      fi
      # `${tag}`, braced. A bare `$tag` here was followed directly by the UTF-8
      # ellipsis, and the shell read that character's leading byte as part of the
      # variable NAME — so under `set -u` this line aborted the whole script with
      # `tag?: unbound variable`. It fired on the FIRST iteration, i.e. on every
      # release where the workflow had not finished yet, which is all of them:
      # v0.2.2 notarized, stapled, signed and wrote its appcast entry, then died
      # here without uploading anything. Braces around any variable that touches
      # a non-ASCII character.
      [ "$waited" = 0 ] && note "waiting for the tag-triggered Release workflow to create ${tag}…"
      sleep 10
      waited=$((waited + 10))
    done
    [ "$waited" -gt 0 ] && note "release appeared after ${waited}s"

    # HIDE the release while it has no appcast, then reveal it once it does.
    #
    # Sparkle's feed is `releases/latest/download/appcast.xml`, so "latest" and
    # "has an appcast" must become true in that order. The tag-triggered workflow
    # publishes a normal (latest) release carrying only CLI tarballs, which leaves
    # a window where the feed URL resolves to a release with no appcast.xml — and
    # GitHub's CDN caches that 404 against the exact URL Sparkle requests. Measured
    # on v0.2.2: the bare URL returned 404 while the same URL with `?cb=1` returned
    # 200, for about two minutes, i.e. every installed copy's update check failed
    # while the release looked fine in the UI.
    #
    # Marking it prerelease first keeps "latest" on the previous good release for
    # the whole upload, so the window never exists.
    gh release edit "$tag" --prerelease --repo "$repo" >/dev/null \
      || die "could not mark $tag prerelease before uploading." \
             "  Uploading anyway would leave the update feed 404ing on a cached miss."

    # --clobber so a re-run replaces its assets rather than failing.
    gh release upload "$tag" "$dmg" "$appcast_path" --clobber --repo "$repo" \
      || die "gh release upload failed for $tag."

    # Only now is it safe to be the newest thing Sparkle looks at.
    gh release edit "$tag" --prerelease=false --latest --repo "$repo" >/dev/null \
      || die "assets uploaded, but $tag is still marked prerelease — Sparkle will never offer it." \
             "  Flip it by hand:" \
             "    gh release edit $tag --prerelease=false --latest --repo $repo"
    note "uploaded to https://github.com/$repo/releases/tag/$tag"
  fi

  report_release_outcome "$tag" "$dry_run"
}

# ---------------------------------------------------------------------------
# Closing message
# ---------------------------------------------------------------------------

# This script performs no git writes — it never commits, branches or pushes
# (see the module header). That is deliberate: main is protected, and this
# checkout routinely holds sibling sessions' uncommitted work, so an
# auto-commit here is a collision hazard, not a convenience. But stage 8 just
# wrote a real <item> into the TRACKED $appcast_path (skipped under
# --dry-run — see appcast_target above), and until now the closing message
# never said so. Two releases in a row read a bare "done", walked away, and
# left the entry stranded — 0.2.15 (c47aa99, #119) and 0.2.17 (PR #121) both
# needed a follow-up commit to land it. release-preflight.sh check 5/6
# already detects the dirty appcast on the NEXT release; that is the safety
# net, not the fix — it fires one release too late to stop the strand. The
# fix is that this script cannot itself claim plain success while it
# happened.
#
# Publishing genuinely succeeded, so this keeps a 0 exit status — turning a
# successful release into a failure exit would be worse than the silence it
# replaces (see the Exit status note in the usage block). It resolves the
# repo root from git itself rather than string-munging $appcast_path, for the
# same reason every other path in this script does.
#
# A standalone function (rather than inline in main) so a fixture can call it
# directly against a real git repo without driving the whole build/sign/
# notarize pipeline.
report_release_outcome() {
  local tag="$1" dry_run="$2" repo_root_for_check appcast_git_status

  # Every other fallible call in this file dies with a message explaining
  # what to do; these two `git` calls did not, so a checkout with no `.git`
  # (or any other git failure) aborted the whole script with a raw `fatal:`
  # and exit 128 from `set -e` — printed as if PUBLISHING had failed, when
  # by this point it had already succeeded. Wrap both explicitly and say so.
  repo_root_for_check="$(git -C "$(dirname "$appcast_path")" rev-parse --show-toplevel 2>&1)" \
    || die "release $tag published, but could not resolve the repo root to check" \
           "whether $appcast_path is committed:" \
           "  $repo_root_for_check" \
           "Check by hand: git -C \"$(dirname "$appcast_path")\" status --porcelain -- \"$appcast_path\""
  appcast_git_status="$(git -C "$repo_root_for_check" status --porcelain -- "$appcast_path" 2>&1)" \
    || die "release $tag published, but 'git status' on $appcast_path failed:" \
           "  $appcast_git_status" \
           "Check by hand: git -C \"$repo_root_for_check\" status --porcelain -- \"$appcast_path\""

  if [ -n "$appcast_git_status" ]; then
    stage "appcast entry left uncommitted"
    note "release $tag published, but $appcast_path is not committed."
    note "This script does not commit, branch, or push — see the module header."
    note "Land it by hand:"
    note "  git -C \"$repo_root_for_check\" commit -- \"$appcast_path\""
    note "  git -C \"$repo_root_for_check\" push && gh pr create --base main"
    printf '\nrelease %s: done, BUT %s is UNCOMMITTED — see above\n' "$tag" "$appcast_path"
  else
    printf '\nrelease %s: done%s\n' "$tag" "$([ "$dry_run" = 1 ] && echo ' (dry run)' || echo '')"
  fi
}

# Guarded so the asserts above can be sourced and exercised in isolation.
if [ "${BASH_SOURCE[0]}" = "${0}" ]; then
  main "$@"
fi
