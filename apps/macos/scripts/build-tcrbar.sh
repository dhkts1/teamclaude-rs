#!/usr/bin/env bash
# Assemble TcrBar.app by hand — no Xcode project, no .xcodeproj to keep in sync.
#
# Output: apps/macos/build/TcrBar.app, ad-hoc signed. Developer ID signing,
# notarization, stapling, DMGs and Sparkle are deliberately out of scope: this is
# a local operator tool, not a distributed product.
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

echo "==> codesign (ad-hoc)"
codesign -s - --force "$app_dir"

echo "built $app_dir  version=$short_version build=$build_number sha=$git_sha"
