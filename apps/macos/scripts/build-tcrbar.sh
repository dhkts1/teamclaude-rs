#!/usr/bin/env bash
# Assemble TcrBar.app by hand — no Xcode project, no .xcodeproj to keep in sync.
#
# Output: apps/macos/build/TcrBar.app, signed with the best certificate present on
# the machine (see the signing ladder below). Notarization, stapling, DMGs and
# Sparkle are deliberately out of scope: this is a local operator tool, not a
# distributed product.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
pkg_dir="$(dirname "$here")"
repo_root="$(cd "$pkg_dir/../.." && pwd)"

app_name="TcrBar"
bundle_id="com.github.dhkts1.tcrbar"
short_version="0.1.0"
build_dir="$pkg_dir/build"
app_dir="$build_dir/$app_name.app"
macos_dir="$app_dir/Contents/MacOS"

# Build stamp. A missing or unreadable .git must never fail the build.
git_sha="unknown"
build_number="0"
if git -C "$repo_root" rev-parse --git-dir >/dev/null 2>&1; then
  git_sha="$(git -C "$repo_root" rev-parse --short HEAD 2>/dev/null || echo unknown)"
  build_number="$(git -C "$repo_root" rev-list --count HEAD 2>/dev/null || echo 0)"
  if [ -n "$(git -C "$repo_root" status --porcelain 2>/dev/null)" ]; then
    git_sha="$git_sha-dirty"
  fi
fi

echo "==> swift build -c release --product $app_name"
swift build --package-path "$pkg_dir" -c release --product "$app_name"
binary="$(swift build --package-path "$pkg_dir" -c release --product "$app_name" --show-bin-path)/$app_name"

echo "==> assembling $app_dir"
rm -rf "$app_dir"
mkdir -p "$macos_dir"
cp "$binary" "$macos_dir/$app_name"

cat >"$app_dir/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleExecutable</key>
	<string>$app_name</string>
	<key>CFBundleIdentifier</key>
	<string>$bundle_id</string>
	<key>CFBundleName</key>
	<string>$app_name</string>
	<key>CFBundlePackageType</key>
	<string>APPL</string>
	<key>CFBundleShortVersionString</key>
	<string>$short_version</string>
	<key>CFBundleVersion</key>
	<string>$build_number</string>
	<key>LSMinimumSystemVersion</key>
	<string>13.0</string>
	<key>LSUIElement</key>
	<true/>
	<key>TcrGitSHA</key>
	<string>$git_sha</string>
</dict>
</plist>
PLIST

# Signing ladder — loudest-safe-first.
#
# The signature is not ceremony here: "Launch at login" registers the bundle with
# macOS by *code identity*. An ad-hoc signature has no certificate behind it, so
# its cdhash changes on every rebuild and a previously-registered login item ends
# up pointing at an identity that no longer matches. The same is true of any
# permission grant the app is ever given.
#
# So: prefer a real certificate. No identity name, team id or email is written
# down here — this repository is public. The label prefix is matched with
# `grep -F` (fixed string, no regex) and the full quoted identity is read out of
# the keychain at build time.
first_identity_matching() {
  # $1 is a certificate-class label prefix, e.g. "Developer ID Application".
  # Lines look like: `  1) <SHA1> "<Class>: <Name> (<TEAM>)"`
  # `|| true`: "no identity of this class" is a normal answer, and under
  # `set -o pipefail` grep's exit 1 would otherwise abort the whole build.
  security find-identity -v -p codesigning 2>/dev/null \
    | grep -F "\"$1: " \
    | head -n 1 \
    | sed -E 's/^[^"]*"(.*)"[[:space:]]*$/\1/' || true
}

sign_identity=""
sign_tier=""
for class in "Developer ID Application" "Apple Development"; do
  sign_identity="$(first_identity_matching "$class")"
  if [ -n "$sign_identity" ]; then
    sign_tier="$class"
    break
  fi
done

if [ -n "$sign_identity" ]; then
  echo "==> codesign ($sign_tier)"
  codesign -s "$sign_identity" --force "$app_dir"
else
  sign_tier="ad-hoc"
  echo "==> codesign (ad-hoc)"
  codesign -s - --force "$app_dir"
  {
    echo
    echo "WARNING: no codesigning certificate found — signed ad-hoc."
    echo "WARNING: an ad-hoc signature has no stable code identity, so its hash"
    echo "WARNING: changes on every rebuild. Consequences:"
    echo "WARNING:   - 'Launch at login' silently stops working after a rebuild,"
    echo "WARNING:     because the registered login item points at an identity"
    echo "WARNING:     that no longer matches this bundle."
    echo "WARNING:   - any future permission grant (Accessibility, Screen"
    echo "WARNING:     Recording, Automation) has to be re-granted each rebuild."
    echo "WARNING: Fix: install a 'Developer ID Application' or 'Apple"
    echo "WARNING: Development' certificate into the login keychain, then rebuild."
    echo
  } >&2
fi

echo "built $app_dir  version=$short_version build=$build_number sha=$git_sha signing=$sign_tier"
