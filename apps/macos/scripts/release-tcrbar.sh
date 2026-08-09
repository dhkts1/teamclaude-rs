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
#   --skip-notarize skip notarization and stapling only. The DMG is still built
#                   and signed; it will be Gatekeeper-quarantined on download.
#   --tag           the release tag. Defaults to v<Cargo.toml version> and must
#                   agree with it — two version numbers for one release is a bug
#                   this script exists to prevent, not to tolerate.
#   --verify-only   run the signature asserts (stages 2 and 3) against an
#                   already-built bundle and exit. Builds nothing.
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
  [ -f "$appcast_path" ] && return 0
  cat >"$appcast_path" <<XML
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
  note "appcast: created $appcast_path"
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
  local item="$1" tmp itemfile before after
  grep -qF "$appcast_marker" "$appcast_path" \
    || die "$appcast_path has no insertion marker. Restore this line inside <channel>:" \
           "  $appcast_marker"
  before="$(grep -c '<item>' "$appcast_path" || true)"
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
  ' "$appcast_path" >"$tmp" || { rm -f "$tmp" "$itemfile"; die "appcast: awk failed to insert the item."; }
  rm -f "$itemfile"
  after="$(grep -c '<item>' "$tmp" || true)"
  [ "$after" -eq $((before + 1)) ] \
    || { rm -f "$tmp"; die "appcast: insert produced $after <item> elements, expected $((before + 1))." \
                           "Refusing to publish a feed that lost or duplicated a release."; }
  mv "$tmp" "$appcast_path"
}

# ---------------------------------------------------------------------------

usage() { sed -n '2,60p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; }

main() {
  local dry_run=0 skip_notarize=0 tag="" verify_only=""
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
    "$dmg" "$app_dir" || [ -f "$dmg" ] || die "create-dmg produced no image at $dmg."
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
  ensure_appcast
  if grep -q "sparkle:shortVersionString=\"$version\"" "$appcast_path"; then
    die "$appcast_path already has an item for version $version." \
        "Bump the version in Cargo.toml rather than republishing one."
  fi
  item="    <item>
      <title>$app_name $version</title>
      <link>https://github.com/$repo/releases/tag/$tag</link>
      <sparkle:version>$build_number</sparkle:version>
      <sparkle:shortVersionString>$version</sparkle:shortVersionString>
      <sparkle:minimumSystemVersion>13.0</sparkle:minimumSystemVersion>
      <pubDate>$pubdate</pubDate>
      <enclosure url=\"$url\" type=\"application/octet-stream\" $sparkle_sig_attrs />
    </item>"
  appcast_insert "$item"
  note "appcast: added $version ($build_number) -> $appcast_path"

  # ---- stage 9: publish ---------------------------------------------------
  if [ "$dry_run" = 1 ]; then
    stage "stage 9/9  publish — SKIPPED (--dry-run)"
    note "would upload $(basename "$dmg") and appcast.xml to $repo release $tag"
  else
    stage "stage 9/9  publish to the GitHub Release"

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
      [ "$waited" = 0 ] && note "waiting for the tag-triggered Release workflow to create $tag…"
      sleep 10
      waited=$((waited + 10))
    done
    [ "$waited" -gt 0 ] && note "release appeared after ${waited}s"

    # --clobber so a re-run replaces its assets rather than failing.
    gh release upload "$tag" "$dmg" "$appcast_path" --clobber --repo "$repo" \
      || die "gh release upload failed for $tag."
    note "uploaded to https://github.com/$repo/releases/tag/$tag"
  fi

  printf '\nrelease %s: done%s\n' "$tag" "$([ "$dry_run" = 1 ] && echo ' (dry run)' || echo '')"
}

# Guarded so the asserts above can be sourced and exercised in isolation.
if [ "${BASH_SOURCE[0]}" = "${0}" ]; then
  main "$@"
fi
