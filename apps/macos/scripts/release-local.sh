#!/usr/bin/env bash
# Cut a signed, notarized TcrBar release from a laptop, with the credentials
# pulled from 1Password at run time.
#
#   apps/macos/scripts/release-local.sh v0.2.0 [--dry-run] [--skip-notarize]
#
# This is a thin credential wrapper. It builds nothing and signs nothing —
# `release-tcrbar.sh` does all of that and stays the single implementation of
# the pipeline. What this script owns is the part that must not be written
# down: getting an App Store Connect API key into the environment and taking it
# back out again.
#
# WHY THIS EXISTS AT ALL. The seven signing secrets were deliberately removed
# from GitHub on 2026-08-09. This repository is public and non-admin
# collaborators hold push; a tag-triggered workflow runs the workflow file as it
# exists on the tagged commit, and Sparkle's private key signs the payload that
# every installed copy auto-trusts. Nothing stored is nothing stolen, so the
# keys live on one Mac and releases are cut from it. See docs/RELEASING.md.
#
# NOTHING HERE PRINTS A SECRET, and nothing here hardcodes a person, a path or
# an account — same rule as release-tcrbar.sh, for the same reason: this file is
# world-readable. The key id and the issuer id are credentials, not labels, so
# the progress line says that credentials loaded, not which ones.
#
# What it needs:
#   - 1Password CLI signed in (1Password app -> Settings -> Developer)
#   - the item named by TCRBAR_OP_ITEM, with the fields read below
#   - Developer ID Application certificate in the login keychain
#   - Sparkle private key in the login keychain (1Password holds the backup)
#
# The certificate and the Sparkle key are read from the keychain by
# `release-tcrbar.sh` itself. What 1Password supplies here is the App Store
# Connect API key, which notarization needs and which exists nowhere else —
# Apple allows exactly one download of the .p8.
#
# Environment:
#   TCRBAR_OP_ITEM   1Password item reference holding the signing fields
set -euo pipefail

tag="${1:-}"
if [ -z "$tag" ]; then
  echo "usage: $(basename "$0") vX.Y.Z [--dry-run] [--skip-notarize]" >&2
  echo "  the tag must match the version in Cargo.toml" >&2
  exit 2
fi
shift || true

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "$here/../../.." && pwd)"
item="${TCRBAR_OP_ITEM:-op://Employee/TcrBar Release Signing}"

command -v op >/dev/null || { echo "ERROR: 1Password CLI (op) not found." >&2; exit 1; }
op whoami >/dev/null 2>&1 || {
  echo "ERROR: 1Password CLI is not signed in." >&2
  echo "       1Password app -> Settings -> Developer -> Integrate with 1Password CLI" >&2
  exit 1
}

# The .p8 has to exist as a FILE because `notarytool` takes --key <path>; there
# is no form of the flag that accepts the key material as a value. So it is
# written 0600 into a private temp dir and removed however this script exits.
# Cleanup is on a trap rather than at the end of the happy path because the
# failure path is the one that matters: a build that aborts halfway is exactly
# when a private key would otherwise be left behind in /tmp.
keydir="$(mktemp -d)"
chmod 700 "$keydir"
cleanup() { rm -rf "$keydir"; }
trap cleanup EXIT INT TERM

keyfile="$keydir/AuthKey.p8"
(umask 077; op read "$item/APPLE_API_KEY_P8" > "$keyfile")
[ -s "$keyfile" ] || { echo "ERROR: the .p8 read back empty from 1Password." >&2; exit 1; }

APPLE_API_KEY_PATH="$keyfile"
APPLE_API_KEY_ID="$(op read "$item/APPLE_API_KEY_ID")"
APPLE_API_ISSUER_ID="$(op read "$item/APPLE_API_ISSUER_ID")"
TCRBAR_SPARKLE_PUBLIC_KEY="$(op read "$item/SPARKLE_ED_PUBLIC_KEY")"
export APPLE_API_KEY_PATH APPLE_API_KEY_ID APPLE_API_ISSUER_ID TCRBAR_SPARKLE_PUBLIC_KEY

# Check every field here rather than three minutes into a build. `op read` on a
# missing field is an error, but a field that exists and is empty is not, and
# that one surfaces as an opaque notarization rejection much later.
for v in APPLE_API_KEY_ID APPLE_API_ISSUER_ID TCRBAR_SPARKLE_PUBLIC_KEY; do
  [ -n "${!v}" ] || { echo "ERROR: $v came back empty from 1Password." >&2; exit 1; }
done

echo "==> credentials loaded from 1Password"
echo "==> releasing $tag from $repo_root"
exec "$repo_root/apps/macos/scripts/release-tcrbar.sh" --tag "$tag" "$@"
